//! `ctxforge warmup` — build the structural graph + FTS index for a repo up front,
//! instead of waiting for the MCP server's lazy first-call build.
//!
//! Writes to the SAME data dir the server reads (`$CTXFORGE_DIR`, else
//! `<cwd>/.ctxforge`), so a server that's already running picks the graph up on its
//! next `graph_query` with no restart (the graph is a plain file it re-reads; the
//! index is WAL SQLite both processes can share).
//!
//! A separate process whose stdout is its own response channel — never the MCP
//! JSON-RPC stream.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::discovery;
use crate::index::Index;
use crate::obs;
use crate::store::Store;

/// `ctxforge warmup [path]` — discover + index `path` (default `.`) into its data dir.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut root = PathBuf::from(".");
    for arg in args {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: ctxforge warmup [path]");
                println!();
                println!("Build the code graph + search index for <path> (default: cwd) into");
                println!("its .ctxforge data dir, so graph_query / ctx_search work immediately.");
                println!("Re-run any time to refresh after the code changes.");
                return Ok(());
            }
            other => root = PathBuf::from(other),
        }
    }
    let data_dir = obs::data_dir();
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    warmup(&root, &data_dir)
}

/// Build the graph (→ `graph.json`) and FTS index for `root`, persisting both into
/// `data_dir` and recording the count stats. Prints a human-readable summary.
pub fn warmup(root: &Path, data_dir: &Path) -> Result<()> {
    // Canonicalize so the index stores the same absolute paths the MCP server uses
    // (its repo_dir is the absolute cwd) — one path scheme across both, and prune
    // can clean up any older mixed-scheme entries.
    let root_buf = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_buf.as_path();
    let store = Store::open(data_dir).context("opening store")?;

    // --- Structural graph (tree-sitter → graph.json) ---
    let outcome = discovery::discover(root, None).context("building code graph")?;
    // Never overwrite a good graph.json with an empty one (no supported source under
    // `root`). Bail so the caller sees the problem instead of a silent empty graph.
    if outcome.response.files_parsed == 0 {
        anyhow::bail!(
            "discover parsed 0 files under {} — refusing to write an empty graph (check the path)",
            root.display()
        );
    }
    let graph_file = data_dir.join("graph.json");
    outcome
        .graph
        .save(&graph_file)
        .with_context(|| format!("writing {}", graph_file.display()))?;
    // Refresh the staleness manifest so the server's lazy `ensure_graph` serves from
    // cache instead of rebuilding what we just built.
    write_manifest(
        &data_dir.join("graph.manifest.json"),
        &discovery::source_manifest(root),
    );
    let g = &outcome.response;
    let _ = store.set_stat("graph_nodes", g.nodes as i64);
    let _ = store.set_stat("graph_edges", g.edges as i64);

    // --- FTS content index ---
    let index = Index::open(data_dir).context("opening index")?;
    let idx = index
        .index_path(root, true)
        .context("indexing repo contents")?;
    // Drop chunks for files deleted since the last build, then record the manifest.
    let _ = index.prune_missing(root);
    if let Ok(total) = index.chunk_count() {
        let _ = store.set_stat("index_chunks", total);
    }
    write_manifest(
        &data_dir.join("index.manifest.json"),
        &crate::index::file_manifest(root),
    );

    let langs = if g.languages.is_empty() {
        "none".to_string()
    } else {
        g.languages.join(", ")
    };
    println!("warmed up {}", root.display());
    println!(
        "  graph : {} nodes, {} edges  ({} files parsed; {langs})",
        g.nodes, g.edges, g.files_parsed
    );
    println!(
        "  index : {} files, {} chunks",
        idx.files_indexed, idx.chunks
    );
    println!("  out   : {}", data_dir.display());
    if !g.warnings.is_empty() {
        println!(
            "  note  : {} file(s) skipped (unparseable)",
            g.warnings.len()
        );
    }
    Ok(())
}

/// Combined staleness signature of a repo: every file's mtime. Any add/edit/delete
/// changes it, which is what triggers a watch rebuild.
fn signature(root: &Path) -> BTreeMap<String, u64> {
    crate::index::file_manifest(root)
}

/// `ctxforge watch [path]` — keep the graph + index fresh in real time as files
/// change, WITHOUT restarting the MCP server. A standalone process that writes to
/// the same data dir the server reads: the server re-reads `graph.json` on its next
/// query and shares the WAL index, so changes appear with no reconnect.
///
/// Polls a cheap mtime manifest (robust on macOS, where FSEvents misses rapid
/// editor saves) and rebuilds once changes have stayed quiet for `debounce`.
pub fn watch(root: &Path, data_dir: &Path, debounce: Duration, poll: Duration) -> Result<()> {
    let root_buf = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let root = root_buf.as_path();
    std::fs::create_dir_all(data_dir).ok();
    println!("ctxforge watch: {}", root.display());
    println!("  data dir : {}", data_dir.display());
    println!(
        "  debounce : {}s   poll : {}s",
        debounce.as_secs(),
        poll.as_secs()
    );
    println!(
        "  rebuilds graph + index on change; the MCP server is never touched (Ctrl-C to stop)"
    );

    if let Err(e) = warmup(root, data_dir) {
        eprintln!("  initial build skipped: {e}");
    }
    let mut last_built = signature(root);
    let mut last_seen = last_built.clone();
    let mut last_change: Option<Instant> = None;

    loop {
        std::thread::sleep(poll);
        let sig = signature(root);
        if sig != last_seen {
            // Something changed since the last poll — (re)arm the quiet timer.
            last_seen = sig;
            last_change = Some(Instant::now());
            continue;
        }
        // No change this tick. Rebuild once a pending change has been quiet long
        // enough and actually differs from what we last built.
        if let Some(t) = last_change {
            if last_seen == last_built {
                last_change = None; // reverted to the built state
            } else if t.elapsed() >= debounce {
                match warmup(root, data_dir) {
                    Ok(()) => {}
                    Err(e) => eprintln!("  rebuild failed: {e}"),
                }
                last_built = signature(root); // re-scan post-build (mtimes stable)
                last_seen = last_built.clone();
                last_change = None;
            }
        }
    }
}

/// CLI entry for `ctxforge watch [path] [--debounce SECS] [--poll SECS]`.
pub fn run_watch_cli(args: &[String]) -> Result<()> {
    let mut root = PathBuf::from(".");
    let mut debounce = Duration::from_secs(3);
    let mut poll = Duration::from_secs(1);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("usage: ctxforge watch [path] [--debounce SECS] [--poll SECS]");
                println!();
                println!("Keep the code graph + search index fresh as files change, writing into");
                println!(
                    "the repo's .ctxforge data dir. A running MCP server picks the changes up"
                );
                println!("on its next query — no restart needed. Defaults: path '.', debounce 3s,");
                println!("poll 1s.");
                return Ok(());
            }
            "--debounce" => {
                if let Some(v) = iter.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        debounce = Duration::from_secs(s);
                    }
                }
            }
            "--poll" => {
                if let Some(v) = iter.next() {
                    if let Ok(s) = v.parse::<u64>() {
                        poll = Duration::from_secs(s.max(1));
                    }
                }
            }
            other => root = PathBuf::from(other),
        }
    }
    // Write where the server reads: $CTXFORGE_DIR, else <repo>/.ctxforge.
    let data_dir = match std::env::var_os("CTXFORGE_DIR") {
        Some(d) => PathBuf::from(d),
        None => std::fs::canonicalize(&root)
            .unwrap_or_else(|_| root.clone())
            .join(".ctxforge"),
    };
    watch(&root, &data_dir, debounce, poll)
}

/// Persist a staleness manifest (best effort; matches the format the server reads).
fn write_manifest(path: &Path, manifest: &BTreeMap<String, u64>) {
    if let Ok(json) = serde_json::to_string(manifest) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn warmup_writes_manifests_and_prunes_deletions() {
        let repo = tempdir().unwrap();
        let data = tempdir().unwrap();
        fs::write(repo.path().join("a.rs"), "fn alpha() {}\n").unwrap();
        fs::write(repo.path().join("b.rs"), "fn beta_unique() {}\n").unwrap();
        warmup(repo.path(), data.path()).unwrap();

        // Manifests written so the server's lazy path sees a fresh cache.
        assert!(data.path().join("graph.manifest.json").exists());
        assert!(data.path().join("index.manifest.json").exists());

        let idx = Index::open(data.path()).unwrap();
        assert!(
            idx.search(&["beta_unique".into()], 5).unwrap().results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("b.rs")),
            "beta_unique searchable before deletion"
        );

        // Delete b.rs, warm up again → graph + index must reflect the deletion.
        fs::remove_file(repo.path().join("b.rs")).unwrap();
        warmup(repo.path(), data.path()).unwrap();

        let idx2 = Index::open(data.path()).unwrap();
        assert!(
            !idx2.search(&["beta_unique".into()], 5).unwrap().results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("b.rs")),
            "deleted file must be pruned from the index"
        );
        let graph = discovery::graph::Graph::load(&data.path().join("graph.json")).unwrap();
        assert!(
            !graph.nodes.iter().any(|n| n.name == "beta_unique"),
            "deleted symbol must be gone from the graph"
        );
    }

    #[test]
    fn warmup_builds_queryable_graph_and_searchable_index() {
        let repo = tempdir().unwrap();
        let data = tempdir().unwrap();
        fs::write(
            repo.path().join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();

        warmup(repo.path(), data.path()).unwrap();

        // Graph persisted and queryable for a symbol.
        let graph_file = data.path().join("graph.json");
        assert!(graph_file.exists(), "graph.json written");
        let graph = discovery::graph::Graph::load(&graph_file).unwrap();
        let view = discovery::query::query(&graph, "helper", None, 10, &[]);
        assert!(
            view.nodes.iter().any(|n| n.name == "helper"),
            "graph has helper"
        );

        // Index populated and searchable.
        let idx = Index::open(data.path()).unwrap();
        assert!(idx.chunk_count().unwrap() > 0, "index has chunks");
        let res = idx.search(&["helper".into()], 5).unwrap();
        assert!(
            res.results[0]
                .hits
                .iter()
                .any(|h| h.path.ends_with("lib.rs")),
            "search finds lib.rs"
        );

        // Count stats recorded for ctx_stats / dashboard.
        let store = Store::open(data.path()).unwrap();
        assert!(store.get_stat("graph_nodes").unwrap() >= 3);
        assert!(store.get_stat("index_chunks").unwrap() >= 1);
    }
}
