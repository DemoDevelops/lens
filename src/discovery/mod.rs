//! `ctx_discover`: walk a repo, parse supported files with tree-sitter, and
//! build a deterministic structural graph written to `.ctxforge/graph.json`.

pub mod extract;
pub mod graph;
pub mod query;

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;

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

    let mut graph = Graph::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut files_parsed = 0usize;
    let mut langs_used: BTreeSet<String> = BTreeSet::new();

    // Raw, cross-file relationships resolved after all nodes exist.
    let mut pending_calls: Vec<(String, String)> = Vec::new();
    let mut pending_imports: Vec<(String, String, usize, String)> = Vec::new(); // (module_id, seg, line, lang)

    for file in &files {
        let ext = match file.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let spec = match extract::spec_for_extension(ext) {
            Some(s) => s,
            None => continue,
        };
        if let Some(filter) = &lang_filter {
            if !filter.contains(spec.name) {
                continue;
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
                Err(_) => continue,
            },
            Err(e) => {
                warnings.push(format!("{rel}: read error: {e}"));
                continue;
            }
        };
        let fx = match extract::extract_file(&rel, &source, &spec) {
            Some(fx) => fx,
            None => {
                warnings.push(format!("{rel}: failed to parse, skipped"));
                continue;
            }
        };

        langs_used.insert(spec.name.to_string());
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
            pending_imports.push((module_id.clone(), seg, line, spec.name.to_string()));
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
}
