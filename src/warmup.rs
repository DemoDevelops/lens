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

use std::path::{Path, PathBuf};

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
    std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    warmup(&root, &data_dir)
}

/// Build the graph (→ `graph.json`) and FTS index for `root`, persisting both into
/// `data_dir` and recording the count stats. Prints a human-readable summary.
pub fn warmup(root: &Path, data_dir: &Path) -> Result<()> {
    let store = Store::open(data_dir).context("opening store")?;

    // --- Structural graph (tree-sitter → graph.json) ---
    let outcome = discovery::discover(root, None).context("building code graph")?;
    let graph_file = data_dir.join("graph.json");
    outcome
        .graph
        .save(&graph_file)
        .with_context(|| format!("writing {}", graph_file.display()))?;
    let g = &outcome.response;
    let _ = store.set_stat("graph_nodes", g.nodes as i64);
    let _ = store.set_stat("graph_edges", g.edges as i64);

    // --- FTS content index ---
    let index = Index::open(data_dir).context("opening index")?;
    let idx = index.index_path(root, true).context("indexing repo contents")?;
    if let Ok(total) = index.chunk_count() {
        let _ = store.set_stat("index_chunks", total);
    }

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
    println!("  index : {} files, {} chunks", idx.files_indexed, idx.chunks);
    println!("  out   : {}", data_dir.display());
    if !g.warnings.is_empty() {
        println!("  note  : {} file(s) skipped (unparseable)", g.warnings.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

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
        let view = discovery::query::query(&graph, "helper", None, 10);
        assert!(view.nodes.iter().any(|n| n.name == "helper"), "graph has helper");

        // Index populated and searchable.
        let idx = Index::open(data.path()).unwrap();
        assert!(idx.chunk_count().unwrap() > 0, "index has chunks");
        let res = idx.search(&["helper".into()], 5).unwrap();
        assert!(
            res.results[0].hits.iter().any(|h| h.path.ends_with("lib.rs")),
            "search finds lib.rs"
        );

        // Count stats recorded for ctx_stats / dashboard.
        let store = Store::open(data.path()).unwrap();
        assert!(store.get_stat("graph_nodes").unwrap() >= 3);
        assert!(store.get_stat("index_chunks").unwrap() >= 1);
    }
}
