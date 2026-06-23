//! `lens_map`: walk a repo, parse supported files with tree-sitter, and
//! build a deterministic structural graph written to `.lens/graph.json`.

pub mod extract;
pub mod graph;
pub mod query;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;

use graph::{Graph, Node};

use crate::tools::DiscoverResponse;

/// Result of a discovery run, including the graph and any per-file warnings.
pub struct DiscoverOutcome {
    pub graph: Graph,
    pub response: DiscoverResponse,
}

/// Discover the structural graph under `root`. `languages` optionally filters to
/// a subset (by language name). The graph is returned; the caller persists it.
pub fn discover(root: &Path, languages: Option<&[String]>) -> Result<DiscoverOutcome> {
    // A non-existent root (commonly a shell-escaped path that survived as a literal,
    // e.g. `AI\ Stuff`) makes the walk silently yield zero files. Fail loudly instead
    // so callers never persist an empty graph over a good one.
    if !root.exists() {
        anyhow::bail!("discover root does not exist: {}", root.display());
    }

    let lang_filter: Option<BTreeSet<String>> =
        languages.map(|ls| ls.iter().map(|l| l.to_ascii_lowercase()).collect());

    // Collect candidate files deterministically.
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    for entry in builder.build().flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(entry.into_path());
        }
    }
    files.sort();

    // Parallel per-file extraction: parse and extract each file concurrently.
    // Each task owns its own Parser; results are keyed by (path, lang_name) for
    // deterministic reassembly regardless of rayon's completion order.
    struct FileResult {
        rel: String,
        lang_name: String,
        fx: extract::FileExtract,
    }

    let (file_results, warnings_raw): (Vec<_>, Vec<_>) = files
        .par_iter()
        .filter_map(|file| {
            let ext = file.extension().and_then(|e| e.to_str())?;
            let spec = extract::spec_for_extension(ext)?;
            if let Some(filter) = &lang_filter {
                if !filter.contains(spec.name) {
                    return None;
                }
            }
            let rel = file
                .strip_prefix(root)
                .unwrap_or(file)
                .to_string_lossy()
                .to_string();
            let source = match std::fs::read(file) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(s) => s,
                    Err(_) => return None,
                },
                Err(e) => return Some(Err(format!("{rel}: read error: {e}"))),
            };
            match extract::extract_file(&rel, &source, &spec) {
                Some(fx) => Some(Ok(FileResult {
                    rel,
                    lang_name: spec.name.to_string(),
                    fx,
                })),
                None => Some(Err(format!("{rel}: failed to parse, skipped"))),
            }
        })
        .partition(Result::is_ok);

    let mut warnings: Vec<String> = warnings_raw
        .into_iter()
        .filter_map(|r| r.err())
        .collect();
    warnings.sort();

    // Sort results by relative path for deterministic assembly order.
    let mut file_results: Vec<FileResult> =
        file_results.into_iter().filter_map(|r| r.ok()).collect();
    file_results.sort_by(|a, b| a.rel.cmp(&b.rel));

    let mut graph = Graph::new();
    let mut files_parsed = 0usize;
    let mut langs_used: BTreeSet<String> = BTreeSet::new();

    // Raw, cross-file relationships resolved after all nodes exist.
    let mut pending_calls: Vec<(String, String)> = Vec::new();
    let mut pending_imports: Vec<(String, String, usize, String)> = Vec::new(); // (module_id, seg, line, lang)

    for FileResult { lang_name, fx, .. } in file_results {
        langs_used.insert(lang_name.clone());
        files_parsed += 1;

        let module_id = fx.module.id.clone();
        graph.add_node(fx.module);
        for d in fx.defs {
            graph.add_node(d);
        }
        for (m, d) in fx.contains {
            graph.add_edge(&m, &d, "contains");
        }
        pending_calls.extend(fx.calls);
        for (seg, line) in fx.imports {
            pending_imports.push((module_id.clone(), seg, line, lang_name.clone()));
        }
    }

    // Resolve calls by callee name within the repo.
    let name_index = graph.name_index();
    for (caller, callee) in pending_calls {
        if let Some(targets) = name_index.get(&callee) {
            for t in targets {
                if *t != caller {
                    graph.add_edge(&caller, t, "calls");
                }
            }
        }
    }

    // Resolve imports: link to a matching repo symbol if present, else create an
    // `import` node so the edge has a real endpoint.
    for (module_id, seg, line, lang) in pending_imports {
        let resolved: Option<String> = name_index
            .get(&seg)
            .and_then(|ids| ids.iter().find(|id| **id != module_id).cloned());
        match resolved {
            Some(target) => graph.add_edge(&module_id, &target, "imports"),
            None => {
                let import_node = Node::new(&seg, "import", &seg, line, &lang);
                let iid = import_node.id.clone();
                graph.add_node(import_node);
                graph.add_edge(&module_id, &iid, "imports");
            }
        }
    }

    // Deterministic ordering of the persisted graph.
    graph.nodes.sort_by(|a, b| a.id.cmp(&b.id));
    graph
        .edges
        .sort_by(|a, b| (&a.from, &a.to, &a.kind).cmp(&(&b.from, &b.to, &b.kind)));

    let response = DiscoverResponse {
        nodes: graph.nodes.len(),
        edges: graph.edges.len(),
        files_parsed,
        languages: langs_used.into_iter().collect(),
        warnings,
    };

    Ok(DiscoverOutcome { graph, response })
}

/// Cheap staleness signature for the graph: every supported source file under
/// `root` mapped to its mtime (ms since epoch). Walked with the SAME filters as
/// [`discover`] (gitignore-respecting, supported extensions only), so comparing
/// it to a saved copy tells us whether the persisted graph is stale. Stat-only —
/// no file reads or parsing — so it is far cheaper than a full discover.
pub fn source_manifest(root: &Path) -> BTreeMap<String, u64> {
    let mut manifest = BTreeMap::new();
    if !root.exists() {
        return manifest;
    }
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    for entry in builder.build().flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        if extract::spec_for_extension(ext).is_none() {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let mtime = std::fs::metadata(path)
            .ok()
            .and_then(|md| md.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        manifest.insert(rel, mtime);
    }
    manifest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn discover_rust_repo_builds_graph() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn helper() -> i32 { 1 }\nfn main() { let _ = helper(); }\n",
        )
        .unwrap();
        let out = discover(dir.path(), None).unwrap();
        assert!(out.response.nodes >= 3); // module + 2 fns
        assert!(out.response.files_parsed >= 1);
        assert!(out.response.languages.contains(&"rust".to_string()));
        // a calls edge between main and helper exists
        assert!(out.graph.edges.iter().any(|e| e.kind == "calls"));
    }

    #[test]
    fn discover_nonexistent_root_errors() {
        // A path that doesn't exist (e.g. a shell-escaped `AI\ Stuff` that survived
        // as a literal) must error, not silently return an empty graph.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("AItestslash\\ Stuff");
        let res = discover(&missing, None);
        assert!(res.is_err(), "nonexistent root must error");
        let err = res.err().unwrap();
        assert!(err.to_string().contains("does not exist"), "got: {err}");
    }

    #[test]
    fn language_filter_excludes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn f() {}\n").unwrap();
        fs::write(dir.path().join("b.py"), "def g():\n    pass\n").unwrap();
        let out = discover(dir.path(), Some(&["python".to_string()])).unwrap();
        assert!(out.response.languages.contains(&"python".to_string()));
        assert!(!out.response.languages.contains(&"rust".to_string()));
    }

    #[test]
    fn deterministic_across_runs() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n",
        )
        .unwrap();
        let g1 = discover(dir.path(), None).unwrap().graph;
        let g2 = discover(dir.path(), None).unwrap().graph;
        let j1 = serde_json::to_string(&g1).unwrap();
        let j2 = serde_json::to_string(&g2).unwrap();
        assert_eq!(j1, j2);
    }

    /// The parallel extract must produce the same graph as the serial result:
    /// identical sorted node ids and identical sorted (from, to, kind) edge triples.
    #[test]
    fn parallel_extract_is_deterministic() {
        let dir = tempdir().unwrap();
        // Multiple files to exercise the parallel path across several workers.
        fs::write(
            dir.path().join("a.rs"),
            "fn alpha() { beta(); }\nfn beta() {}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("b.rs"),
            "fn gamma() { alpha(); }\nfn delta() { gamma(); }\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("c.py"),
            "def epsilon():\n    return 1\n\ndef zeta():\n    epsilon()\n",
        )
        .unwrap();

        // Run discover three times; all should agree on sorted nodes and edges.
        let run = || {
            let g = discover(dir.path(), None).unwrap().graph;
            let mut node_ids: Vec<String> = g.nodes.iter().map(|n| n.id.clone()).collect();
            node_ids.sort();
            let mut edge_keys: Vec<(String, String, String)> = g
                .edges
                .iter()
                .map(|e| (e.from.clone(), e.to.clone(), e.kind.clone()))
                .collect();
            edge_keys.sort();
            (node_ids, edge_keys)
        };

        let r1 = run();
        let r2 = run();
        let r3 = run();
        assert_eq!(r1, r2, "run 1 vs run 2 differ");
        assert_eq!(r1, r3, "run 1 vs run 3 differ");
        // Sanity: we got nodes from all three files.
        assert!(r1.0.len() >= 3, "expected at least 3 nodes, got {}", r1.0.len());
    }
}
