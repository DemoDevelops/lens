//! `lens_map`: walk a repo, parse supported files with tree-sitter, and
//! build a deterministic structural graph written to `.lens/graph.json`.

pub mod extract;
pub mod graph;
pub mod query;
pub mod structural;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;
use tree_sitter::Tree;

use extract::FileExtract;
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

    let file_results: Vec<FileResult> = file_results.into_iter().filter_map(|r| r.ok()).collect();

    Ok(assemble_graph(file_results, warnings))
}

/// One file's extraction plus the bookkeeping the graph assembly needs. Keyed by
/// relative path for deterministic reassembly regardless of completion order.
struct FileResult {
    rel: String,
    lang_name: String,
    fx: extract::FileExtract,
}

/// Assemble the whole [`Graph`] from per-file extracts. This is the single,
/// deterministic assembly path used by BOTH the full [`discover`] and the
/// incremental rediscovery: it sorts the per-file results by relative path, adds
/// every node/edge in that fixed order, resolves cross-file calls/imports against
/// the same name index, and applies the same final node/edge sort. Because the
/// output depends ONLY on the set of `FileResult`s (not on how each was parsed),
/// feeding it the same extracts always yields a byte-identical graph — which is
/// how the incremental path stays identical to a from-scratch rebuild.
fn assemble_graph(mut file_results: Vec<FileResult>, warnings: Vec<String>) -> DiscoverOutcome {
    // Sort results by relative path for deterministic assembly order.
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

    // Resolve calls scope-aware. For each callee name, prefer a definition in the
    // SAME file as the caller; only when there is none fall back to the repo-wide
    // name index (an imported symbol or a same-name def elsewhere). Same-file
    // narrowing drops the spurious cross-file edges a pure name index produces when
    // a name is reused across files, without losing any real edge. The graph
    // borrows are scoped so the resolved list can be added back mutably.
    let name_index = graph.name_index();
    let resolved_calls: Vec<(String, String)> = {
        // name -> node ids, scoped per file (built from the nodes already added).
        let mut defs_by_file: HashMap<&str, HashMap<&str, Vec<&str>>> = HashMap::new();
        for n in &graph.nodes {
            defs_by_file
                .entry(n.file.as_str())
                .or_default()
                .entry(n.name.as_str())
                .or_default()
                .push(n.id.as_str());
        }
        let caller_file: HashMap<&str, &str> = graph
            .nodes
            .iter()
            .map(|n| (n.id.as_str(), n.file.as_str()))
            .collect();
        let mut out: Vec<(String, String)> = Vec::new();
        for (caller, callee) in &pending_calls {
            let file = match caller_file.get(caller.as_str()) {
                Some(f) => *f,
                None => continue,
            };
            let same_file: Vec<&str> = defs_by_file
                .get(file)
                .and_then(|m| m.get(callee.as_str()))
                .map(|ids| {
                    ids.iter()
                        .copied()
                        .filter(|id| *id != caller.as_str())
                        .collect()
                })
                .unwrap_or_default();
            if !same_file.is_empty() {
                for t in same_file {
                    out.push((caller.clone(), t.to_string()));
                }
            } else if let Some(targets) = name_index.get(callee) {
                for t in targets {
                    if t != caller {
                        out.push((caller.clone(), t.clone()));
                    }
                }
            }
        }
        out
    };
    for (caller, t) in resolved_calls {
        graph.add_edge(&caller, &t, "calls");
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

    DiscoverOutcome { graph, response }
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

/// One file's cached parse: the source bytes it was parsed from, the resulting
/// tree-sitter `Tree` (reused as the base for the next incremental parse), and the
/// extracted symbols/relationships. `Tree` is `Send + Sync` and `FileExtract` holds
/// only owned primitives, so the whole cache is shareable behind an `Arc<RwLock>`.
pub struct CachedFile {
    /// blake3 of the source bytes the cache entry was built from.
    pub hash: [u8; 32],
    /// The source text the `tree` was parsed from, retained so the next
    /// incremental reparse can compute the byte delta against it.
    pub source: String,
    pub tree: Tree,
    pub extract: FileExtract,
}

/// Per-file parse cache keyed by relative path. Owned by the caller (the server's
/// `Forge`), passed in so [`discover_incremental`] can reuse unchanged files and
/// incrementally re-parse changed ones.
pub type ParseCache = HashMap<String, CachedFile>;

/// Outcome of an incremental rediscovery: the graph (byte-identical to a full
/// [`discover`]) plus how many files actually required a (re)parse this run.
pub struct IncrementalOutcome {
    pub graph: Graph,
    pub response: DiscoverResponse,
    /// Number of files parsed from disk this run (changed + new). Unchanged files
    /// served from `cache` are NOT counted.
    pub files_reparsed: usize,
}

/// Rediscover `root`, reparsing ONLY files whose content changed since the last
/// run and reusing cached extracts for the rest. `cache` is updated in place:
/// changed/new files get a fresh `(hash, tree, extract)`, deleted files are
/// dropped. The graph is then assembled by the SAME [`assemble_graph`] path the
/// full [`discover`] uses, over every file's current `FileExtract`, so the result
/// is byte-identical to a from-scratch `discover` of the post-edit tree.
///
/// `languages` filters by language name exactly as [`discover`] does.
pub fn discover_incremental(
    root: &Path,
    languages: Option<&[String]>,
    cache: &mut ParseCache,
) -> Result<IncrementalOutcome> {
    if !root.exists() {
        anyhow::bail!("discover root does not exist: {}", root.display());
    }

    let lang_filter: Option<BTreeSet<String>> =
        languages.map(|ls| ls.iter().map(|l| l.to_ascii_lowercase()).collect());

    // Collect candidate files deterministically (same walk as `discover`).
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    for entry in builder.build().flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            files.push(entry.into_path());
        }
    }
    files.sort();

    let mut file_results: Vec<FileResult> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut files_reparsed = 0usize;
    // Relative paths of supported source files seen this run; cache entries not in
    // this set are deleted files and get pruned below.
    let mut live: BTreeSet<String> = BTreeSet::new();

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
                Err(_) => continue, // non-UTF8: skipped, exactly like `discover`
            },
            Err(e) => {
                warnings.push(format!("{rel}: read error: {e}"));
                continue;
            }
        };
        live.insert(rel.clone());
        let hash: [u8; 32] = *blake3::hash(source.as_bytes()).as_bytes();

        // Unchanged: reuse the cached extract verbatim (no parse).
        if let Some(cached) = cache.get(&rel) {
            if cached.hash == hash {
                file_results.push(FileResult {
                    rel,
                    lang_name: spec.name.to_string(),
                    fx: cached.extract.clone(),
                });
                continue;
            }
        }

        // Changed (have a prior tree) → incremental reparse from the retained old
        // source + tree; new file → fresh parse.
        let parsed = match cache.remove(&rel) {
            Some(prev) => {
                extract::reparse_incremental(&rel, &prev.source, &source, prev.tree, &spec)
            }
            None => extract::extract_file_with_tree(&rel, &source, &spec),
        };
        match parsed {
            Some((fx, tree)) => {
                files_reparsed += 1;
                cache.insert(
                    rel.clone(),
                    CachedFile {
                        hash,
                        source: source.clone(),
                        tree,
                        extract: fx.clone(),
                    },
                );
                file_results.push(FileResult {
                    rel,
                    lang_name: spec.name.to_string(),
                    fx,
                });
            }
            None => warnings.push(format!("{rel}: failed to parse, skipped")),
        }
    }

    // Prune cache entries for files that no longer exist (deletions).
    cache.retain(|path, _| live.contains(path));

    warnings.sort();
    let DiscoverOutcome { graph, response } = assemble_graph(file_results, warnings);
    Ok(IncrementalOutcome {
        graph,
        response,
        files_reparsed,
    })
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

    /// THE hard gate for T11: the incremental rediscovery path must produce a graph
    /// BYTE-IDENTICAL to a full from-scratch `discover` of the same on-disk tree, for
    /// an edit, an add, and a delete — and must reparse only the files that changed.
    ///
    /// Byte-identity is checked as exact equality of the serialized graph (the form
    /// persisted to graph.json), which pins both node and edge content AND order.
    #[test]
    fn incremental_reparse_is_byte_identical_to_full_rebuild() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        let c = dir.path().join("c.py");
        fs::write(&a, "fn alpha() { beta(); }\nfn beta() {}\n").unwrap();
        fs::write(&b, "fn gamma() { alpha(); }\nfn delta() { gamma(); }\n").unwrap();
        fs::write(&c, "def epsilon():\n    return 1\n\ndef zeta():\n    epsilon()\n").unwrap();

        let json = |g: &Graph| serde_json::to_string(g).unwrap();

        // Prime the cache with a full incremental run over the initial tree. Every
        // file is new, so all three parse.
        let mut cache = ParseCache::new();
        let primed = discover_incremental(dir.path(), None, &mut cache).unwrap();
        assert_eq!(primed.files_reparsed, 3, "initial run parses every file");
        assert_eq!(
            json(&primed.graph),
            json(&discover(dir.path(), None).unwrap().graph),
            "primed incremental graph must equal a full rebuild"
        );

        // (a) EDIT exactly one file. Only it must reparse, and the graph must equal a
        // full rebuild of the edited tree.
        fs::write(&a, "fn alpha() { delta(); }\nfn beta() { alpha(); }\n").unwrap();
        let edited = discover_incremental(dir.path(), None, &mut cache).unwrap();
        assert_eq!(
            edited.files_reparsed, 1,
            "a 1-file edit must reparse exactly 1 file, got {}",
            edited.files_reparsed
        );
        assert_eq!(
            json(&edited.graph),
            json(&discover(dir.path(), None).unwrap().graph),
            "incremental graph after a 1-file edit must be byte-identical to a full rebuild"
        );

        // (b) ADD a file. Only the new file parses; the graph still matches a full
        // rebuild.
        let d = dir.path().join("d.rs");
        fs::write(&d, "fn omega() { alpha(); }\n").unwrap();
        let added = discover_incremental(dir.path(), None, &mut cache).unwrap();
        assert_eq!(added.files_reparsed, 1, "adding 1 file reparses exactly 1");
        assert_eq!(
            json(&added.graph),
            json(&discover(dir.path(), None).unwrap().graph),
            "incremental graph after an add must be byte-identical to a full rebuild"
        );

        // (c) DELETE a file. Nothing reparses (no changed/new content), the cache
        // entry is pruned, and the graph still matches a full rebuild.
        fs::remove_file(&b).unwrap();
        let deleted = discover_incremental(dir.path(), None, &mut cache).unwrap();
        assert_eq!(
            deleted.files_reparsed, 0,
            "a pure deletion reparses no files, got {}",
            deleted.files_reparsed
        );
        assert!(
            !cache.contains_key("b.rs"),
            "deleted file must be pruned from the cache"
        );
        assert_eq!(
            json(&deleted.graph),
            json(&discover(dir.path(), None).unwrap().graph),
            "incremental graph after a delete must be byte-identical to a full rebuild"
        );
    }

    /// Measured (not a gate): full `discover` of lens's own `src/` vs an incremental
    /// rediscovery after a single 1-byte edit. Reports the speedup. Run with
    /// `--nocapture` to see it.
    #[test]
    fn measure_incremental_vs_full_on_src() {
        use std::time::Instant;

        // lens's own source tree (this test runs from the crate root).
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        if !src.exists() {
            return; // be robust if run from an unusual layout
        }

        // Full build timing (cold parse of every file).
        let t = Instant::now();
        let full = discover(&src, None).unwrap();
        let full_ms = t.elapsed();

        // Prime the cache, then make a tiny edit to one file in a temp copy is heavy;
        // instead drive the SAME tree but flip one cached file's hash so exactly one
        // file is treated as changed and incrementally reparsed. This isolates the
        // per-edit cost (1 reparse + reuse-the-rest + assemble) from FS churn.
        let mut cache = ParseCache::new();
        let _ = discover_incremental(&src, None, &mut cache).unwrap();
        if let Some((_k, v)) = cache.iter_mut().next() {
            v.hash = [0u8; 32]; // force a single-file reparse next run
        }
        let t = Instant::now();
        let inc = discover_incremental(&src, None, &mut cache).unwrap();
        let inc_ms = t.elapsed();

        let ratio = full_ms.as_secs_f64() / inc_ms.as_secs_f64().max(f64::MIN_POSITIVE);
        println!(
            "[T11 measured] full discover(src) = {:?} ({} nodes); incremental after 1-file \
             change = {:?} (files_reparsed={}); full/incremental ratio = {:.1}x",
            full_ms, full.response.nodes, inc_ms, inc.files_reparsed, ratio
        );
        assert_eq!(inc.files_reparsed, 1, "exactly one file should reparse");
    }
}
