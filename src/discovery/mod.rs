//! `lens_map`: walk a repo, parse supported files with tree-sitter, and
//! build a deterministic structural graph written to `.lens/graph.json`.

pub mod extract;
pub mod graph;
pub mod query;
pub mod skeleton;
pub mod structural;
pub mod tags_adapter;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use anyhow::Result;
use ignore::WalkBuilder;
use rayon::prelude::*;
use tree_sitter::Tree;

use extract::{FileExtract, MdLink, MdLinkKind};
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
            let spec = tags_adapter::any_spec_for_extension(ext)?;
            if let Some(filter) = &lang_filter {
                if !filter.contains(spec.name()) {
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
            match spec.extract_file(&rel, &source) {
                Some(fx) => Some(Ok(FileResult {
                    rel,
                    lang_name: spec.name().to_string(),
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
    let mut pending_md_links: Vec<(String, MdLink)> = Vec::new(); // (linking module_id, link)
    let mut pending_tags: Vec<(String, String)> = Vec::new(); // (module_id, tag)
    let mut pending_aliases: Vec<(String, String)> = Vec::new(); // (declaring module_id, alias)

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
        for link in fx.md_links {
            pending_md_links.push((module_id.clone(), link));
        }
        for tag in fx.md_tags {
            pending_tags.push((module_id.clone(), tag));
        }
        for alias in fx.md_aliases {
            pending_aliases.push((module_id.clone(), alias));
        }
    }

    let name_index = graph.name_index();

    // Per-file, per-name imported source files. For a `use`/`import` that resolves
    // to a real definition, record (importing_file, imported_name) -> the file that
    // DEFINES it. Used by call resolution to prefer the imported definition's file
    // before the repo-wide fallback. Built from the SAME name-index resolution the
    // import edges use, so the preferred file is exactly the import target's file.
    // BTreeSet keeps the per-key file set deterministic.
    let module_file: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "module")
        .map(|n| (n.id.as_str(), n.file.as_str()))
        .collect();
    let node_file: HashMap<&str, &str> = graph
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.file.as_str()))
        .collect();
    let mut imported_src: HashMap<(String, String), BTreeSet<String>> = HashMap::new();
    for (module_id, seg, _line, _lang) in &pending_imports {
        let resolved: Option<&String> = name_index
            .get(seg)
            .and_then(|ids| ids.iter().find(|id| **id != *module_id));
        if let (Some(target), Some(imp_file)) =
            (resolved, module_file.get(module_id.as_str()).copied())
        {
            if let Some(src_file) = node_file.get(target.as_str()).copied() {
                imported_src
                    .entry((imp_file.to_string(), seg.clone()))
                    .or_default()
                    .insert(src_file.to_string());
            }
        }
    }

    // Resolve calls scope-aware. For each callee name, prefer a definition in the
    // SAME file as the caller; if none, prefer a definition in the file the caller
    // IMPORTED that name from; only then fall back to the repo-wide name index.
    // Same-file narrowing drops spurious cross-file edges from a reused name;
    // import-file narrowing drops the spurious edges to OTHER files that happen to
    // define the same name when the call is to an imported symbol. The graph borrows
    // are scoped so the resolved list can be added back mutably.
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
                continue;
            }
            // Prefer the file(s) the callee name was imported from in this file.
            let import_scoped: Vec<&String> = match imported_src
                .get(&(file.to_string(), callee.clone()))
            {
                Some(src_files) => name_index
                    .get(callee)
                    .map(|ids| {
                        ids.iter()
                            .filter(|t| **t != *caller)
                            .filter(|t| {
                                node_file
                                    .get(t.as_str())
                                    .map(|f| src_files.contains(*f))
                                    .unwrap_or(false)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            };
            if !import_scoped.is_empty() {
                for t in import_scoped {
                    out.push((caller.clone(), t.clone()));
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

    // Frontmatter `tags:` become shared `kind:"tag"` nodes (one per distinct tag
    // name across the whole graph — [`Graph::add_node`] dedups by id, which for a tag
    // is derived from the name) with a `tagged` edge from each declaring module.
    // Markdown-only: `pending_tags` is empty for a pure code repo.
    for (module_id, tag) in pending_tags {
        let tag_node = Node::new(&tag, "tag", &tag, 1, "markdown");
        let tag_id = tag_node.id.clone();
        graph.add_node(tag_node);
        graph.add_edge(&module_id, &tag_id, "tagged");
    }

    // Resolve markdown cross-doc links into `imports` edges. Markdown-only:
    // `pending_md_links` is empty for a pure code repo, so this is a no-op there and
    // the code graph stays byte-identical to before this pass existed. Frontmatter
    // `aliases:` feed name-based (wikilink/reference) resolution.
    resolve_md_links(&mut graph, pending_md_links, pending_aliases);

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

/// Resolve markdown cross-doc links into `imports` edges, and `#anchor`s into edges
/// to the specific HEADING node they target. Runs ONLY over markdown links
/// (`pending_md_links` is empty for a code-only repo), so it never perturbs the code
/// graph. Two resolution rules, chosen by link syntax:
///
/// * `Inline` (`[t](path)`): the path is resolved RELATIVE to the linking file's
///   directory and matched EXACTLY against a module node's rel-path name — a
///   universally safe, path-based match. `index.md`'s `[deploy](./deploy.md)` links
///   to the `deploy.md` module node itself, not a bare `deploy` stub.
/// * `Wikilink` (`[[note]]`): the target basename is matched against every markdown
///   module's file stem AND against frontmatter `aliases:` keys (so `[[oldname]]`
///   also resolves to a module declaring `oldname` as an alias); on a collision it
///   links ALL matches, in deterministic id order.
/// * `Embed` (`![[note]]`): resolved exactly like a `Wikilink` (stem/alias/anchor),
///   but emitted as an `embeds` edge instead of `imports`.
///
/// Path-based matching keys off each module's `.file` (its rel path, which never
/// changes) rather than its display `.name`, so a frontmatter `title:` can rename a
/// module without breaking inline/wikilink resolution.
///
/// A link matching nothing keeps a synthetic `import` stub node (the "unresolved
/// link" marker) so the edge still has a real endpoint.
///
/// When a link carries a `#anchor`, the edge targets the specific HEADING node in
/// the resolved file whose GitHub slug ([`slug`]) matches the anchor, instead of the
/// module node — `[[arch#Overview]]` links to arch.md's `Overview` heading, not
/// arch.md itself. A same-doc anchor (`[jump](#setup)`, empty target) resolves
/// within the LINKING file. If the anchor doesn't match any heading in the resolved
/// file, the pass falls back to the module-level edge (never drops the link).
fn resolve_md_links(
    graph: &mut Graph,
    pending_md_links: Vec<(String, MdLink)>,
    pending_aliases: Vec<(String, String)>,
) {
    if pending_md_links.is_empty() {
        return;
    }

    // Index the markdown module nodes: by exact rel-path (inline resolution) and by
    // file stem (wikilink resolution). Also index every heading node by (file,
    // slug(name)) so an anchored link can find its specific heading. Built into
    // owned Strings so the immutable borrow of `graph.nodes` is released before the
    // graph is mutated below.
    let mut module_by_path: HashMap<String, String> = HashMap::new();
    let mut modules_by_stem: HashMap<String, Vec<String>> = HashMap::new();
    let mut module_file: HashMap<String, String> = HashMap::new();
    let mut headings_by_file_slug: HashMap<(String, String), Vec<String>> = HashMap::new();
    for n in &graph.nodes {
        if n.language != "markdown" {
            continue;
        }
        if n.kind == "module" {
            // Key path-based lookups off `.file` (the rel path), NOT `.name`: a
            // frontmatter `title:` renames `.name`, but the rel path a link resolves
            // to is stable. `.file == .name` for a module with no title.
            module_by_path.insert(n.file.clone(), n.id.clone());
            modules_by_stem
                .entry(file_stem(&n.file).to_string())
                .or_default()
                .push(n.id.clone());
            module_file.insert(n.id.clone(), n.file.clone());
        } else if n.kind == "heading" {
            headings_by_file_slug
                .entry((n.file.clone(), slug(&n.name)))
                .or_default()
                .push(n.id.clone());
        }
    }
    for ids in modules_by_stem.values_mut() {
        ids.sort();
    }
    for ids in headings_by_file_slug.values_mut() {
        ids.sort();
    }

    // Alias basename -> declaring module ids, keyed by the same file-stem
    // normalization the link side uses, so a `[[oldname]]` (or a reference resolving
    // to a bare `oldname`) also reaches the module that declares that alias.
    let mut alias_index: HashMap<String, Vec<String>> = HashMap::new();
    for (module_id, alias) in pending_aliases {
        alias_index
            .entry(file_stem(&alias).to_string())
            .or_default()
            .push(module_id);
    }
    for ids in alias_index.values_mut() {
        ids.sort();
        ids.dedup();
    }

    // Compute the edges and stubs first, then apply them, so no read borrow of the
    // graph is held across a mutation.
    let mut edges: Vec<(String, String)> = Vec::new();
    let mut stubs: Vec<(String, String, usize)> = Vec::new(); // (from_module, stub_name, line)
    // Transclusion embeds resolve exactly like wikilinks but drain to `embeds` edges,
    // so they collect separately.
    let mut embed_edges: Vec<(String, String)> = Vec::new();
    let mut embed_stubs: Vec<(String, String, usize)> = Vec::new();
    for (module_id, link) in pending_md_links {
        let file = match module_file.get(&module_id) {
            Some(f) => f.as_str(),
            None => continue,
        };
        let dir = parent_dir(file);
        match link.kind {
            MdLinkKind::Inline => {
                if link.target.is_empty() {
                    // Same-doc `#anchor` link: resolve within the linking file.
                    if let Some(heads) =
                        resolve_anchor_headings(&headings_by_file_slug, file, link.anchor.as_deref())
                    {
                        for h in heads {
                            if *h != module_id {
                                edges.push((module_id.clone(), h.clone()));
                            }
                        }
                    }
                    continue;
                }
                let canonical = resolve_relative(dir, &link.target);
                match module_by_path.get(&canonical) {
                    Some(target) if *target != module_id => {
                        let target_file =
                            module_file.get(target).map(String::as_str).unwrap_or_default();
                        match resolve_anchor_headings(
                            &headings_by_file_slug,
                            target_file,
                            link.anchor.as_deref(),
                        ) {
                            Some(heads) => {
                                edges.extend(heads.iter().map(|h| (module_id.clone(), h.clone())))
                            }
                            None => edges.push((module_id.clone(), target.clone())),
                        }
                    }
                    Some(_) => {
                        // Link to self: only meaningful with an anchor, resolved in
                        // this file; an anchorless self-link stays a no-op.
                        if let Some(heads) =
                            resolve_anchor_headings(&headings_by_file_slug, file, link.anchor.as_deref())
                        {
                            for h in heads {
                                if *h != module_id {
                                    edges.push((module_id.clone(), h.clone()));
                                }
                            }
                        }
                    }
                    None => {
                        // Exact path missed. A reference/inline target that is a bare
                        // alias name (no matching file) still resolves via the alias
                        // index; otherwise it stays an unresolved-link stub.
                        let stem = file_stem(&canonical).to_string();
                        match alias_index.get(&stem) {
                            Some(ids) => {
                                for target in ids {
                                    if *target == module_id {
                                        continue;
                                    }
                                    let target_file = module_file
                                        .get(target)
                                        .map(String::as_str)
                                        .unwrap_or_default();
                                    match resolve_anchor_headings(
                                        &headings_by_file_slug,
                                        target_file,
                                        link.anchor.as_deref(),
                                    ) {
                                        Some(heads) => edges.extend(
                                            heads.iter().map(|h| (module_id.clone(), h.clone())),
                                        ),
                                        None => edges.push((module_id.clone(), target.clone())),
                                    }
                                }
                            }
                            None => stubs.push((module_id.clone(), canonical, link.line)),
                        }
                    }
                }
            }
            // A wikilink (`[[note]]`) resolves by file stem/alias to an `imports`
            // edge; a transclusion embed (`![[note]]`) resolves identically but to an
            // `embeds` edge — same logic, different drain target.
            MdLinkKind::Wikilink => resolve_wikilike(
                &module_id,
                file,
                &link,
                &modules_by_stem,
                &alias_index,
                &module_file,
                &headings_by_file_slug,
                &mut edges,
                &mut stubs,
            ),
            MdLinkKind::Embed => resolve_wikilike(
                &module_id,
                file,
                &link,
                &modules_by_stem,
                &alias_index,
                &module_file,
                &headings_by_file_slug,
                &mut embed_edges,
                &mut embed_stubs,
            ),
            // Reference is resolved as Inline at extraction, never constructed here.
            MdLinkKind::Reference => {}
        }
    }

    for (from, name, line) in stubs {
        let stub = Node::new(&name, "import", &name, line, "markdown");
        let sid = stub.id.clone();
        graph.add_node(stub);
        graph.add_edge(&from, &sid, "imports");
    }
    // An unmatched embed keeps the same unresolved-link `import` stub marker as the
    // other link kinds, but its edge stays `embeds` so the link's syntax is preserved.
    for (from, name, line) in embed_stubs {
        let stub = Node::new(&name, "import", &name, line, "markdown");
        let sid = stub.id.clone();
        graph.add_node(stub);
        graph.add_edge(&from, &sid, "embeds");
    }
    for (from, to) in edges {
        graph.add_edge(&from, &to, "imports");
    }
    for (from, to) in embed_edges {
        graph.add_edge(&from, &to, "embeds");
    }
}

/// Resolve a wikilink-shaped target (`[[note]]` / `![[note]]`) by file stem and
/// declared alias, identically for a `Wikilink` and an `Embed`; only the edge KIND
/// the caller drains `edges`/`stubs` with differs. Union the stem and alias matches,
/// dedup, and link every one in deterministic id order; an `#anchor` narrows to the
/// specific heading node; a self-target resolves its anchor within the linking file;
/// a total miss pushes an unresolved-link stub. Resolved endpoints go to `edges`,
/// misses to `stubs`, so the caller emits each with the right edge kind.
#[allow(clippy::too_many_arguments)]
fn resolve_wikilike(
    module_id: &str,
    file: &str,
    link: &MdLink,
    modules_by_stem: &HashMap<String, Vec<String>>,
    alias_index: &HashMap<String, Vec<String>>,
    module_file: &HashMap<String, String>,
    headings_by_file_slug: &HashMap<(String, String), Vec<String>>,
    edges: &mut Vec<(String, String)>,
    stubs: &mut Vec<(String, String, usize)>,
) {
    if link.target.is_empty() {
        return;
    }
    let stem = file_stem(&link.target).to_string();
    let mut targets: Vec<String> = Vec::new();
    if let Some(ids) = modules_by_stem.get(&stem) {
        targets.extend(ids.iter().cloned());
    }
    if let Some(ids) = alias_index.get(&stem) {
        targets.extend(ids.iter().cloned());
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        stubs.push((module_id.to_string(), link.target.clone(), link.line));
        return;
    }
    for target in &targets {
        if target.as_str() == module_id {
            if let Some(heads) =
                resolve_anchor_headings(headings_by_file_slug, file, link.anchor.as_deref())
            {
                for h in heads {
                    if h.as_str() != module_id {
                        edges.push((module_id.to_string(), h.clone()));
                    }
                }
            }
            continue;
        }
        let target_file = module_file.get(target).map(String::as_str).unwrap_or_default();
        match resolve_anchor_headings(headings_by_file_slug, target_file, link.anchor.as_deref()) {
            Some(heads) => edges.extend(heads.iter().map(|h| (module_id.to_string(), h.clone()))),
            None => edges.push((module_id.to_string(), target.clone())),
        }
    }
}

/// Heading ids in `file` whose GitHub [`slug`] matches `anchor`, or `None` if
/// `anchor` is absent or matches no heading in that file — the caller's cue to fall
/// back to a module-level edge instead of dropping the link.
fn resolve_anchor_headings<'a>(
    headings_by_file_slug: &'a HashMap<(String, String), Vec<String>>,
    file: &str,
    anchor: Option<&str>,
) -> Option<&'a Vec<String>> {
    let anchor = anchor?;
    headings_by_file_slug.get(&(file.to_string(), slug(anchor)))
}

/// The directory portion of a `/`-separated relative path (`sub/util.md` -> `sub`,
/// `index.md` -> ``).
fn parent_dir(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// Resolve a `/`-separated relative `target` against `dir`, collapsing `.`/`..`
/// components. `("", "./deploy.md")` -> `deploy.md`; `("sub", "../util.md")` ->
/// `util.md`; `("sub", "peer.md")` -> `sub/peer.md`.
fn resolve_relative(dir: &str, target: &str) -> String {
    let mut stack: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    for comp in target.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    stack.join("/")
}

/// The file stem of a `/`-separated path: its last component with a trailing `.md`
/// or `.markdown` extension removed (`sub/util.md` -> `util`, `arch` -> `arch`).
fn file_stem(path: &str) -> &str {
    let base = path.rsplit('/').next().unwrap_or(path);
    base.strip_suffix(".md")
        .or_else(|| base.strip_suffix(".markdown"))
        .unwrap_or(base)
}

/// GitHub-style heading slug: lowercase, drop punctuation (keep `-`), spaces become
/// `-`, and consecutive `-` collapse to one. `slug("Overview") == "overview"`;
/// `slug("My Setup!") == "my-setup"`.
fn slug(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut prev_dash = false;
    for ch in text.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if (lower == '-' || lower.is_whitespace()) && !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    if out.ends_with('-') {
        out.pop();
    }
    out
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
        if tags_adapter::any_spec_for_extension(ext).is_none() {
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
        let spec = match tags_adapter::any_spec_for_extension(ext) {
            Some(s) => s,
            None => continue,
        };
        if let Some(filter) = &lang_filter {
            if !filter.contains(spec.name()) {
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
                    lang_name: spec.name().to_string(),
                    fx: cached.extract.clone(),
                });
                continue;
            }
        }

        // Changed (have a prior tree) → incremental reparse from the retained old
        // source + tree; new file → fresh parse.
        let parsed = match cache.remove(&rel) {
            Some(prev) => {
                spec.reparse_incremental(&rel, &prev.source, &source, prev.tree)
            }
            None => spec.extract_file_with_tree(&rel, &source),
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
                    lang_name: spec.name().to_string(),
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

    /// Cross-file call precision: a call whose name matches a symbol IMPORTED from
    /// one file must NOT also link to same-named definitions in unrelated files.
    /// `main.rs::caller` calls `run`, imported from `helpers.rs`; `run` is also
    /// defined in `unrelated.rs`. The only correct `calls` edge from `caller` is to
    /// `helpers.rs::run`. Reported precision = correct / total caller->run edges.
    #[test]
    fn cross_file_call_precision() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("main.rs"),
            "use crate::helpers::run;\n\npub fn caller() {\n    run();\n}\n",
        )
        .unwrap();
        fs::write(dir.path().join("helpers.rs"), "pub fn run() {}\n").unwrap();
        fs::write(
            dir.path().join("unrelated.rs"),
            "pub fn run() {}\npub fn other() { run(); }\n",
        )
        .unwrap();

        let g = discover(dir.path(), None).unwrap().graph;
        let info = |id: &str| g.node(id).map(|n| (n.name.clone(), n.file.clone()));
        // All caller(main.rs) -> run edges.
        let run_edges: Vec<(String, String)> = g
            .edges
            .iter()
            .filter(|e| e.kind == "calls")
            .filter_map(|e| {
                let (fname, ffile) = info(&e.from)?;
                let (tname, tfile) = info(&e.to)?;
                if fname == "caller" && ffile == "main.rs" && tname == "run" {
                    Some((fname, tfile))
                } else {
                    None
                }
            })
            .collect();
        let total = run_edges.len();
        let correct = run_edges
            .iter()
            .filter(|(_, tfile)| tfile == "helpers.rs")
            .count();
        let precision = if total == 0 {
            0.0
        } else {
            correct as f64 / total as f64
        };
        let recall: f64 = if correct >= 1 { 1.0 } else { 0.0 };
        println!(
            "[L15 cross-file] caller->run edges total={total} correct={correct} \
             precision={precision:.3} recall={recall:.3}"
        );
        // After the fix: exactly one edge, to the imported source file.
        assert_eq!(total, 1, "expected exactly one caller->run edge");
        assert!((precision - 1.0).abs() < 1e-9, "precision must be 1.0");
        assert!((recall - 1.0).abs() < 1e-9, "recall must be 1.0");
    }

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

    /// T1 core: markdown inline and wikilink links must resolve to the REAL target
    /// MODULE nodes (connect-the-docs), not collapse onto bare-name `import` stubs.
    #[test]
    fn markdown_links_resolve_to_module_nodes() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("index.md"),
            "# Index\n\nSee [deploy](./deploy.md) and [[arch#Overview]].\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("deploy.md"),
            "# Deploy\n\nBack to [home](./index.md).\n",
        )
        .unwrap();
        fs::write(dir.path().join("arch.md"), "# Arch\n\n## Overview\n").unwrap();

        let g = discover(dir.path(), None).unwrap().graph;

        let module_id = |file: &str| -> String {
            g.nodes
                .iter()
                .find(|n| n.kind == "module" && n.file == file)
                .unwrap_or_else(|| panic!("no module node for {file}"))
                .id
                .clone()
        };
        let idx = module_id("index.md");
        let dep = module_id("deploy.md");
        let arch = module_id("arch.md");
        let has_import = |from: &str, to: &str| {
            g.edges
                .iter()
                .any(|e| e.kind == "imports" && e.from == from && e.to == to)
        };

        // Inline `[deploy](./deploy.md)` -> the real deploy.md MODULE node.
        assert!(
            has_import(&idx, &dep),
            "index.md -> deploy.md import missing; edges={:?}",
            g.edges
        );
        // Wikilink `[[arch#Overview]]` -> arch.md's `Overview` HEADING node (T4: the
        // anchor resolves to the specific heading, not the module).
        let overview = g
            .nodes
            .iter()
            .find(|n| n.kind == "heading" && n.file == "arch.md" && n.name == "Overview")
            .unwrap_or_else(|| panic!("no Overview heading node in arch.md; nodes={:?}", g.nodes))
            .id
            .clone();
        assert!(
            has_import(&idx, &overview),
            "index.md -> arch.md#Overview heading import missing; edges={:?}",
            g.edges
        );
        assert!(
            !has_import(&idx, &arch),
            "an anchored link that resolves to a heading must not ALSO edge to the module"
        );
        // Backlink `[home](./index.md)` from deploy.md -> index.md.
        assert!(
            has_import(&dep, &idx),
            "deploy.md -> index.md backlink missing; edges={:?}",
            g.edges
        );

        // Every link resolved, so no synthetic bare-name `import` stub was minted.
        let stubs: Vec<&str> = g
            .nodes
            .iter()
            .filter(|n| n.kind == "import")
            .map(|n| n.name.as_str())
            .collect();
        assert!(
            stubs.is_empty(),
            "resolved markdown links must not leave import stubs; got {stubs:?}"
        );
    }

    /// T4: a link's `#anchor` resolves to the specific HEADING node (matched by
    /// GitHub slug), not the module — for both a wikilink (`[[arch#Overview]]`) and
    /// an inline link (`[x](arch.md#overview)`) — and a same-doc anchor
    /// (`[jump](#setup)`) resolves within the linking file itself.
    #[test]
    fn markdown_anchors_resolve_to_heading_nodes() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("index.md"),
            "# Index\n\nSee [[arch#Overview]], [x](arch.md#overview), and [jump](#setup).\n\n\
             ## Setup\n\nSetup content.\n",
        )
        .unwrap();
        fs::write(dir.path().join("arch.md"), "# Arch\n\n## Overview\n").unwrap();

        let g = discover(dir.path(), None).unwrap().graph;

        let module_id = |file: &str| -> String {
            g.nodes
                .iter()
                .find(|n| n.kind == "module" && n.file == file)
                .unwrap_or_else(|| panic!("no module node for {file}"))
                .id
                .clone()
        };
        let heading_id = |file: &str, name: &str| -> String {
            g.nodes
                .iter()
                .find(|n| n.kind == "heading" && n.file == file && n.name == name)
                .unwrap_or_else(|| panic!("no heading node {name:?} in {file}"))
                .id
                .clone()
        };
        let has_import = |from: &str, to: &str| {
            g.edges
                .iter()
                .any(|e| e.kind == "imports" && e.from == from && e.to == to)
        };

        let idx = module_id("index.md");
        let arch = module_id("arch.md");
        let arch_overview = heading_id("arch.md", "Overview");
        let idx_setup = heading_id("index.md", "Setup");

        // Both `[[arch#Overview]]` and `[x](arch.md#overview)` land on arch.md's
        // Overview HEADING node, not the arch MODULE node.
        assert!(
            has_import(&idx, &arch_overview),
            "index.md -> arch.md#Overview heading edge missing; edges={:?}",
            g.edges
        );
        assert!(
            !has_import(&idx, &arch),
            "an anchored link that resolves to a heading must not ALSO edge to the module"
        );

        // Same-doc `[jump](#setup)` resolves within index.md itself.
        assert!(
            has_import(&idx, &idx_setup),
            "index.md -> #setup same-doc heading edge missing; edges={:?}",
            g.edges
        );
    }

    /// T5: frontmatter drives three graph facts. `title:` renames the module node;
    /// `tags:` mints a shared `kind:"tag"` node with a `tagged` edge; `aliases:` lets
    /// a `[[oldname]]` from another note resolve to the aliased module (an `imports`
    /// edge), even though no file is named `oldname`.
    #[test]
    fn markdown_frontmatter_title_tags_aliases() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("note.md"),
            "---\ntitle: My Note\ntags: [alpha]\naliases: [oldname]\n---\n# Body\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("other.md"),
            "# Other\n\nSee [[oldname]].\n",
        )
        .unwrap();

        let g = discover(dir.path(), None).unwrap().graph;

        let module_id = |file: &str| -> String {
            g.nodes
                .iter()
                .find(|n| n.kind == "module" && n.file == file)
                .unwrap_or_else(|| panic!("no module node for {file}"))
                .id
                .clone()
        };
        let note = module_id("note.md");
        let other = module_id("other.md");

        // `title:` renamed the module node (its `.file` stays note.md).
        let note_name = g.node(&note).unwrap().name.clone();
        assert_eq!(note_name, "My Note", "title must rename the module node");

        // `tags: [alpha]` -> a shared `kind:"tag"` node + a `tagged` edge from note.
        let tag = g
            .nodes
            .iter()
            .find(|n| n.kind == "tag" && n.name == "alpha")
            .unwrap_or_else(|| panic!("no tag node `alpha`; nodes={:?}", g.nodes))
            .id
            .clone();
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == "tagged" && e.from == note && e.to == tag),
            "note.md -> tag `alpha` `tagged` edge missing; edges={:?}",
            g.edges
        );

        // `aliases: [oldname]` -> `[[oldname]]` in other.md resolves to note.md.
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == "imports" && e.from == other && e.to == note),
            "other.md -> note.md alias `imports` edge missing; edges={:?}",
            g.edges
        );
        // The alias link resolved to a real module, so no bare-name stub was minted.
        assert!(
            !g.nodes.iter().any(|n| n.kind == "import" && n.name == "oldname"),
            "alias link must not leave an `oldname` import stub; nodes={:?}",
            g.nodes
        );
    }

    /// T6(a): a transclusion embed `![[deploy]]` resolves to the target MODULE node
    /// via an `embeds`-kind edge (not `imports`), leaving no bare-name stub.
    #[test]
    fn markdown_embed_resolves_to_embeds_edge() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("notes.md"),
            "# Notes\n\nSee the guide:\n\n![[deploy]]\n",
        )
        .unwrap();
        fs::write(dir.path().join("deploy.md"), "# Deploy\n").unwrap();

        let g = discover(dir.path(), None).unwrap().graph;
        let module_id = |file: &str| -> String {
            g.nodes
                .iter()
                .find(|n| n.kind == "module" && n.file == file)
                .unwrap_or_else(|| panic!("no module node for {file}"))
                .id
                .clone()
        };
        let notes = module_id("notes.md");
        let dep = module_id("deploy.md");
        let has_edge = |from: &str, to: &str, kind: &str| {
            g.edges
                .iter()
                .any(|e| e.kind == kind && e.from == from && e.to == to)
        };

        assert!(
            has_edge(&notes, &dep, "embeds"),
            "notes.md -> deploy.md `embeds` edge missing; edges={:?}",
            g.edges
        );
        // An embed is an `embeds` edge, never an `imports` edge.
        assert!(
            !has_edge(&notes, &dep, "imports"),
            "an embed must not also produce an imports edge; edges={:?}",
            g.edges
        );
        // The embed resolved to a real module, so no stub node was minted.
        assert!(
            !g.nodes.iter().any(|n| n.kind == "import"),
            "resolved embed must not leave an import stub; nodes={:?}",
            g.nodes
        );
    }

    /// T6(b): an inline `#alpha` hashtag in prose mints a shared `kind:"tag"` node and
    /// a `tagged` edge from the note, reusing the same path as frontmatter tags.
    #[test]
    fn markdown_inline_tag_makes_tagged_edge() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("notes.md"),
            "# Notes\n\nSome #alpha findings worth keeping.\n",
        )
        .unwrap();

        let g = discover(dir.path(), None).unwrap().graph;
        let notes = g
            .nodes
            .iter()
            .find(|n| n.kind == "module" && n.file == "notes.md")
            .unwrap()
            .id
            .clone();
        let tag = g
            .nodes
            .iter()
            .find(|n| n.kind == "tag" && n.name == "alpha")
            .unwrap_or_else(|| panic!("no tag node `alpha`; nodes={:?}", g.nodes))
            .id
            .clone();
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == "tagged" && e.from == notes && e.to == tag),
            "notes.md -> tag `alpha` `tagged` edge missing; edges={:?}",
            g.edges
        );
    }

    /// THE T6 no-regression gate (graph half): a plain CommonMark note (an inline
    /// link + headings, but NO `[[`, NO `![[`, NO `#tag`) contributes ZERO `tagged`
    /// and ZERO `embeds` edges. This proves PKM name-based resolution never engages on
    /// a plain repo. (The extract half lives in `discovery::extract`.)
    #[test]
    fn markdown_plain_note_has_no_pkm_edges() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("plain.md"),
            "# Plain CommonMark\n\nA normal inline link to [architecture](./arch.md).\n\n\
             ## Content\n\nRegular prose with no special hashtag syntax for tags.\n\n\
             ## Structure\n\n- Item one\n- Item two\n",
        )
        .unwrap();
        fs::write(dir.path().join("arch.md"), "# Arch\n\nArchitecture notes.\n").unwrap();

        let g = discover(dir.path(), None).unwrap().graph;

        assert!(
            !g.edges.iter().any(|e| e.kind == "tagged"),
            "a plain note must contribute no `tagged` edges; edges={:?}",
            g.edges
        );
        assert!(
            !g.edges.iter().any(|e| e.kind == "embeds"),
            "a plain note must contribute no `embeds` edges; edges={:?}",
            g.edges
        );
        assert!(
            !g.nodes.iter().any(|n| n.kind == "tag"),
            "a plain note must mint no `tag` nodes; nodes={:?}",
            g.nodes
        );
        // Sanity: the plain inline link still resolves to the real arch.md module
        // (standard CommonMark is owned by lens; only PKM syntax is gated off).
        let plain = g
            .nodes
            .iter()
            .find(|n| n.kind == "module" && n.file == "plain.md")
            .unwrap()
            .id
            .clone();
        let arch = g
            .nodes
            .iter()
            .find(|n| n.kind == "module" && n.file == "arch.md")
            .unwrap()
            .id
            .clone();
        assert!(
            g.edges
                .iter()
                .any(|e| e.kind == "imports" && e.from == plain && e.to == arch),
            "plain.md -> arch.md inline `imports` edge missing; edges={:?}",
            g.edges
        );
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
