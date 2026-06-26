//! Generic `tags.scm` adapter: graph extraction for any tree-sitter grammar that
//! ships a `TAGS_QUERY` constant, with no per-language query authoring.
//!
//! Every grammar crate exposes a `tags.scm` (the query GitHub code-nav uses) as a
//! `pub const TAGS_QUERY`. Its captures follow a fixed shape:
//!   - `@name`            the identifier node naming the symbol (defs AND refs)
//!   - `@definition.<k>`  marks the enclosing node as a definition of kind `<k>`
//!   - `@reference.call` / `@reference.send`  marks a call / method-send site
//! This adapter consumes that shape to produce the same [`FileExtract`] the
//! hand-written specs produce, so adding a language is a [`TagsLangSpec`] registry
//! entry plus a `Cargo.toml` dependency, nothing more.
//!
//! Quality envelope (intentionally coarser than a hand-written spec): tags.scm
//! collapses kinds (Rust struct/enum/union/type all become `class`, trait becomes
//! `interface`) and carries no imports. A language is "promoted" later by setting
//! [`TagsLangSpec::imports_query`] and refining kinds, gated by its fixture test.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor, Tree};

use super::extract::{
    enclosing_scope, import_targets, input_edit, last_segment, node_text, FileExtract, LangSpec,
};
use super::graph::Node;

/// How to extract one language via its grammar's `TAGS_QUERY`.
#[derive(Clone, Copy)]
pub struct TagsLangSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub language: fn() -> Language,
    /// The grammar's `TAGS_QUERY` (`tags.scm`) source.
    pub tags_query: &'static str,
    /// Optional import query (tags.scm has none). `None` ⇒ no import edges. The
    /// capture marks an import statement node, exactly like a hand-written
    /// `imports_query`.
    pub imports_query: Option<&'static str>,
}

/// Unified dispatch over hand-written and tags-based specs, used at the discovery
/// chokepoint so `lens_map` graphs both. Keeps the 6 hand-written specs (and every
/// other consumer of [`LangSpec`]) byte-for-byte untouched: hand-written wins on a
/// shared extension; tags-based fills the gap.
pub enum AnySpec {
    Hand(LangSpec),
    Tags(TagsLangSpec),
}

impl AnySpec {
    pub fn name(&self) -> &'static str {
        match self {
            AnySpec::Hand(s) => s.name,
            AnySpec::Tags(s) => s.name,
        }
    }

    pub fn extract_file(&self, path: &str, source: &str) -> Option<FileExtract> {
        match self {
            AnySpec::Hand(s) => super::extract::extract_file(path, source, s),
            AnySpec::Tags(s) => extract_tags_file(path, source, s),
        }
    }

    pub fn extract_file_with_tree(
        &self,
        path: &str,
        source: &str,
    ) -> Option<(FileExtract, Tree)> {
        match self {
            AnySpec::Hand(s) => super::extract::extract_file_with_tree(path, source, s),
            AnySpec::Tags(s) => extract_tags_file_with_tree(path, source, s),
        }
    }

    pub fn reparse_incremental(
        &self,
        path: &str,
        old_source: &str,
        new_source: &str,
        old_tree: Tree,
    ) -> Option<(FileExtract, Tree)> {
        match self {
            AnySpec::Hand(s) => {
                super::extract::reparse_incremental(path, old_source, new_source, old_tree, s)
            }
            AnySpec::Tags(s) => {
                reparse_tags_incremental(path, old_source, new_source, old_tree, s)
            }
        }
    }
}

/// Resolve an extension to an extractor: hand-written specs first (precision), then
/// the tags registry. This is the single dispatch chokepoint the discovery walk uses.
pub fn any_spec_for_extension(ext: &str) -> Option<AnySpec> {
    if let Some(s) = super::extract::spec_for_extension(ext) {
        return Some(AnySpec::Hand(s));
    }
    tags_spec_for_extension(ext).map(AnySpec::Tags)
}

/// Tags spec for an extension, if one is registered.
pub fn tags_spec_for_extension(ext: &str) -> Option<TagsLangSpec> {
    tags_registry().into_iter().find(|s| s.extensions.contains(&ext))
}

/// The tags-based language registry. New languages are added here (plus a
/// `Cargo.toml` dependency and a fixture test). Grouped by wave so merges of
/// per-group work touch disjoint regions of this vec.
pub fn tags_registry() -> Vec<TagsLangSpec> {
    vec![
        // --- Wave 2 / T4: C-family ---
        TagsLangSpec {
            name: "c",
            extensions: &["c"],
            language: || tree_sitter_c::LANGUAGE.into(),
            tags_query: tree_sitter_c::TAGS_QUERY,
            imports_query: None,
        },
        TagsLangSpec {
            name: "cpp",
            extensions: &["cpp", "cc", "cxx", "hpp", "hh", "h"],
            language: || tree_sitter_cpp::LANGUAGE.into(),
            tags_query: tree_sitter_cpp::TAGS_QUERY,
            imports_query: None,
        },
        TagsLangSpec {
            name: "csharp",
            extensions: &["cs"],
            language: || tree_sitter_c_sharp::LANGUAGE.into(),
            tags_query: tree_sitter_c_sharp::TAGS_QUERY,
            imports_query: None,
        },
        // --- Wave 2 / T5: JVM ---
        // --- Wave 2 / T6: scripting ---
        // --- Wave 2 / T7: Roku ---
        // --- Wave 2 / T8: config that maps ---
    ]
}

/// Tags specs for the two languages lens ALSO hand-writes (Rust, Python), i.e. the
/// only ones where a hand-written oracle exists to calibrate the adapter against.
/// Used solely by `bench_calibration`; NOT dispatched (the hand-written specs own
/// `rs`/`py`, so these never shadow them).
pub fn oracle_tags_specs() -> Vec<TagsLangSpec> {
    vec![
        TagsLangSpec {
            name: "rust",
            extensions: &["rs"],
            language: || tree_sitter_rust::LANGUAGE.into(),
            tags_query: tree_sitter_rust::TAGS_QUERY,
            imports_query: None,
        },
        TagsLangSpec {
            name: "python",
            extensions: &["py"],
            language: || tree_sitter_python::LANGUAGE.into(),
            tags_query: tree_sitter_python::TAGS_QUERY,
            imports_query: None,
        },
    ]
}

/// Parse `source` and extract via the grammar's tags query.
pub fn extract_tags_file(path: &str, source: &str, spec: &TagsLangSpec) -> Option<FileExtract> {
    extract_tags_file_with_tree(path, source, spec).map(|(fx, _)| fx)
}

/// Like [`extract_tags_file`] but also returns the parsed tree for incremental reuse.
pub fn extract_tags_file_with_tree(
    path: &str,
    source: &str,
    spec: &TagsLangSpec,
) -> Option<(FileExtract, Tree)> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let fx = extract_tags_from_tree(path, source, spec, &tree)?;
    Some((fx, tree))
}

/// Incremental reparse for a tags language (mirrors `extract::reparse_incremental`):
/// edit the old tree by the byte delta, reparse reusing it, re-extract. The returned
/// tree is a complete correct parse of `new_source` regardless of the edit's precision.
pub fn reparse_tags_incremental(
    path: &str,
    old_source: &str,
    new_source: &str,
    mut old_tree: Tree,
    spec: &TagsLangSpec,
) -> Option<(FileExtract, Tree)> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let edit = input_edit(old_source, new_source);
    old_tree.edit(&edit);
    let tree = parser.parse(new_source, Some(&old_tree))?;
    let fx = extract_tags_from_tree(path, new_source, spec, &tree)?;
    Some((fx, tree))
}

/// Core tags walk: turn `@definition.*`/`@name`/`@reference.call` captures into the
/// same [`FileExtract`] shape the hand-written path produces.
fn extract_tags_from_tree(
    path: &str,
    source: &str,
    spec: &TagsLangSpec,
    tree: &Tree,
) -> Option<FileExtract> {
    let root = tree.root_node();
    let src = source.as_bytes();
    let module = Node::new(path, "module", path, 1, spec.name);

    let query = cached_tags_query(spec)?;
    let qref: &Query = &query;
    let cap_names = qref.capture_names();

    // --- pass 1: definitions ---
    // A def pattern captures @name (the identifier) and @definition.<kind> (the
    // enclosing node). Some grammars match one node under two def patterns (Rust:
    // an impl method matches both @definition.method and @definition.function), so
    // dedup by the @name node, keeping the most specific kind.
    struct DefCand {
        start: usize,
        kind: String,
        name: String,
        line: usize,
        def_ast_id: usize,
        def_ast_kind: &'static str,
    }
    let mut by_name: HashMap<usize, DefCand> = HashMap::new();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(qref, root, src);
    while let Some(m) = it.next() {
        let mut name_node: Option<TsNode> = None;
        let mut def: Option<(TsNode, &str)> = None;
        for c in m.captures {
            let cn = cap_names[c.index as usize];
            if cn == "name" {
                name_node = Some(c.node);
            } else if let Some(suffix) = cn.strip_prefix("definition.") {
                def = Some((c.node, suffix));
            }
        }
        if let (Some(nn), Some((dn, suffix))) = (name_node, def) {
            let name = node_text(&nn, src);
            if name.is_empty() {
                continue;
            }
            let cand = DefCand {
                start: nn.start_byte(),
                kind: suffix.to_string(),
                name,
                line: nn.start_position().row + 1,
                def_ast_id: dn.id(),
                def_ast_kind: dn.kind(),
            };
            match by_name.get(&nn.id()) {
                Some(prev) if tag_kind_priority(&prev.kind) >= tag_kind_priority(&cand.kind) => {}
                _ => {
                    by_name.insert(nn.id(), cand);
                }
            }
        }
    }

    // Emit defs in source order for determinism.
    let mut cands: Vec<DefCand> = by_name.into_values().collect();
    cands.sort_by(|a, b| (a.start, &a.name).cmp(&(b.start, &b.name)));
    let mut defs: Vec<Node> = Vec::with_capacity(cands.len());
    let mut contains: Vec<(String, String)> = Vec::new();
    // Map AST def-node id -> graph node id, for function-like scopes only.
    let mut scope_map: HashMap<usize, String> = HashMap::new();
    let mut scope_kinds: BTreeSet<&'static str> = BTreeSet::new();
    for c in &cands {
        let node = Node::new(path, &c.kind, &c.name, c.line, spec.name);
        let nid = node.id.clone();
        contains.push((module.id.clone(), nid.clone()));
        if matches!(c.kind.as_str(), "function" | "method") {
            scope_map.insert(c.def_ast_id, nid.clone());
            scope_kinds.insert(c.def_ast_kind);
        }
        defs.push(node);
    }
    let scope_kinds: Vec<&str> = scope_kinds.into_iter().collect();

    // --- pass 2: calls (@reference.call) ---
    // Built after pass 1 so enclosing_scope can resolve a call's caller.
    let mut calls: Vec<(String, String)> = Vec::new();
    let mut ccur = QueryCursor::new();
    let mut cit = ccur.matches(qref, root, src);
    while let Some(m) = cit.next() {
        let mut name_node: Option<TsNode> = None;
        let mut is_call = false;
        for c in m.captures {
            let cn = cap_names[c.index as usize];
            if cn == "name" {
                name_node = Some(c.node);
            } else if cn == "reference.call" || cn == "reference.send" {
                // .call = function call (Rust/Python/C); .send = method/message send
                // (Ruby/C#/Objective-C). Both are invocations -> a `calls` edge.
                is_call = true;
            }
        }
        if !is_call {
            continue;
        }
        let nn = match name_node {
            Some(n) => n,
            None => continue,
        };
        let callee = last_segment(&node_text(&nn, src));
        if callee.is_empty() {
            continue;
        }
        let caller = enclosing_scope(&nn, &scope_map, &scope_kinds)
            .unwrap_or_else(|| module.id.clone());
        calls.push((caller, callee));
    }

    // --- imports (only if the spec opts in) ---
    let mut imports: Vec<(String, usize)> = Vec::new();
    if let Some(iq_src) = spec.imports_query {
        if let Some(iq) = cached_imports_query(spec.name, spec.language, iq_src) {
            let mut icur = QueryCursor::new();
            let mut iit = icur.matches(&iq, root, src);
            while let Some(m) = iit.next() {
                for c in m.captures {
                    let line = c.node.start_position().row + 1;
                    let text = node_text(&c.node, src);
                    for seg in import_targets(&text) {
                        imports.push((seg, line));
                    }
                }
            }
        }
    }

    Some(FileExtract {
        module,
        defs,
        calls,
        imports,
        contains,
    })
}

/// Specificity of a tags def kind, for deduping a node matched by two def patterns.
/// `method` beats `function` (Rust impl methods match both); everything else is
/// unambiguous (distinct AST node kinds never collide on the same @name node).
fn tag_kind_priority(kind: &str) -> u8 {
    match kind {
        "method" => 2,
        "function" => 1,
        _ => 0,
    }
}

/// Process-wide cache of compiled tags queries, keyed by language name. A failed
/// compile is negative-cached (`None`) so a broken grammar is not recompiled per
/// file. `Query` is Send + Sync, so the cache is safe across the parallel walk.
fn cached_tags_query(spec: &TagsLangSpec) -> Option<Arc<Query>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<Arc<Query>>>>> = OnceLock::new();
    compile_cached(
        CACHE.get_or_init(|| Mutex::new(HashMap::new())),
        spec.name,
        spec.language,
        spec.tags_query,
    )
}

/// As [`cached_tags_query`] but for an opt-in imports query (separate cache so the
/// key (the language name) does not collide with the tags query).
fn cached_imports_query(
    name: &'static str,
    language: fn() -> Language,
    src: &'static str,
) -> Option<Arc<Query>> {
    static CACHE: OnceLock<Mutex<HashMap<&'static str, Option<Arc<Query>>>>> = OnceLock::new();
    compile_cached(
        CACHE.get_or_init(|| Mutex::new(HashMap::new())),
        name,
        language,
        src,
    )
}

fn compile_cached(
    cache: &Mutex<HashMap<&'static str, Option<Arc<Query>>>>,
    key: &'static str,
    language: fn() -> Language,
    src: &str,
) -> Option<Arc<Query>> {
    let mut map = cache.lock().unwrap();
    if let Some(slot) = map.get(key) {
        return slot.clone();
    }
    let lang = language();
    let compiled = Query::new(&lang, src).ok().map(Arc::new);
    map.insert(key, compiled.clone());
    compiled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_tags_spec() -> TagsLangSpec {
        TagsLangSpec {
            name: "rust",
            extensions: &["rs"],
            language: || tree_sitter_rust::LANGUAGE.into(),
            tags_query: tree_sitter_rust::TAGS_QUERY,
            imports_query: None,
        }
    }

    fn kinds(fx: &FileExtract) -> Vec<String> {
        let mut k: Vec<String> = fx.defs.iter().map(|n| n.kind.clone()).collect();
        k.sort();
        k.dedup();
        k
    }

    /// T1 predicate: a Rust file extracted purely via `TAGS_QUERY` yields
    /// function/class kinds (tags collapses `struct` -> `class`) and a call edge.
    #[test]
    fn rust_via_tags_query() {
        let src = r#"
struct Widget { x: i32 }

fn helper() -> i32 { 42 }

fn main() {
    let w = helper();
    println!("{}", w);
}
"#;
        let spec = rust_tags_spec();
        let fx = extract_tags_file("a.rs", src, &spec).unwrap();

        let ks = kinds(&fx);
        assert!(ks.contains(&"function".to_string()), "kinds: {ks:?}"); // helper, main
        assert!(ks.contains(&"class".to_string()), "kinds: {ks:?}"); // struct Widget

        let names: Vec<&str> = fx.defs.iter().map(|n| n.name.as_str()).collect();
        for want in ["helper", "main", "Widget"] {
            assert!(names.contains(&want), "missing def {want}; got {names:?}");
        }

        // The keystone edge: main calls helper.
        assert!(
            fx.calls.iter().any(|(_, c)| c == "helper"),
            "calls: {:?}",
            fx.calls
        );
    }

    /// The double-match wrinkle: a Rust impl method matches both
    /// `@definition.method` and `@definition.function`. It must appear exactly once,
    /// as `method`, and a call in its body must attribute to it (not the module).
    #[test]
    fn rust_tags_dedups_impl_methods() {
        let src = r#"
struct S;
impl S {
    fn m(&self) { helper(); }
}
fn helper() {}
"#;
        let fx = extract_tags_file("b.rs", src, &rust_tags_spec()).unwrap();

        let m_kinds: Vec<&str> = fx
            .defs
            .iter()
            .filter(|n| n.name == "m")
            .map(|n| n.kind.as_str())
            .collect();
        assert_eq!(m_kinds, vec!["method"], "method `m` once, as method: {m_kinds:?}");

        // helper() is called from inside method m; the edge must exist and its caller
        // must be a def node (m), not the module fallback.
        let m_id = fx.defs.iter().find(|n| n.name == "m").map(|n| n.id.clone());
        let helper_edges: Vec<&String> = fx
            .calls
            .iter()
            .filter(|(_, c)| c == "helper")
            .map(|(caller, _)| caller)
            .collect();
        assert!(!helper_edges.is_empty(), "calls: {:?}", fx.calls);
        assert!(
            helper_edges.iter().any(|caller| Some((*caller).clone()) == m_id),
            "helper() should attribute to method m; calls: {:?}",
            fx.calls
        );
    }

    /// Empty registry today; the seam exists for the language waves. Guards against a
    /// spec being registered with an extension a hand-written spec already owns
    /// (which dispatch would shadow, silently dropping the tags entry).
    #[test]
    fn registry_does_not_shadow_handwritten() {
        for spec in tags_registry() {
            for ext in spec.extensions {
                assert!(
                    super::super::extract::spec_for_extension(ext).is_none(),
                    "tags lang {} claims extension {ext:?} already owned by a hand-written spec",
                    spec.name
                );
            }
        }
    }

    // --- Per-language fixtures: each proves the registered grammar extracts a named
    // def and a call edge through the generic adapter (the real T4-T8 predicate). ---

    fn extracts(ext: &str, src: &str) -> FileExtract {
        let spec = tags_spec_for_extension(ext).unwrap_or_else(|| panic!("no tags spec for .{ext}"));
        extract_tags_file(&format!("fixture.{ext}"), src, &spec)
            .unwrap_or_else(|| panic!(".{ext} failed to parse/extract"))
    }
    fn def_names(fx: &FileExtract) -> Vec<&str> {
        fx.defs.iter().map(|n| n.name.as_str()).collect()
    }
    fn has_def(fx: &FileExtract, name: &str) -> bool {
        fx.defs.iter().any(|n| n.name == name)
    }
    fn has_call(fx: &FileExtract, name: &str) -> bool {
        fx.calls.iter().any(|(_, c)| c == name)
    }

    #[test]
    fn c_extraction() {
        // C tags.scm captures definitions only (no @reference.call); calls stay empty.
        let fx = extracts(
            "c",
            "int helper(int x) { return x + 1; }\nint add(int a, int b) { return helper(a) + b; }\n",
        );
        assert!(has_def(&fx, "add") && has_def(&fx, "helper"), "defs: {:?}", def_names(&fx));
    }

    #[test]
    fn cpp_extraction() {
        // C++ tags.scm captures definitions only (no references); calls stay empty.
        let fx = extracts(
            "cpp",
            "int helper() { return 1; }\nclass Widget { public: int render() { return helper(); } };\n",
        );
        assert!(
            has_def(&fx, "helper") && has_def(&fx, "render") && has_def(&fx, "Widget"),
            "defs: {:?}",
            def_names(&fx)
        );
    }

    #[test]
    fn csharp_extraction() {
        // C# tags.scm captures method sends on member access (this.Helper()) via
        // @reference.send; a bare Helper() is not a tagged reference.
        let fx = extracts(
            "cs",
            "class Widget { int Render() { return this.Helper(); } int Helper() { return 1; } }\n",
        );
        assert!(has_def(&fx, "Render") && has_def(&fx, "Helper"), "defs: {:?}", def_names(&fx));
        assert!(has_call(&fx, "Helper"), "calls: {:?}", fx.calls);
    }
}
