//! tree-sitter parsing: source file -> symbols + raw relationships.
//!
//! Each language is described by a [`LangSpec`]: the grammar plus three queries
//! (definitions, calls, imports) and the AST node kinds that count as a callable
//! scope. The 6 hand-written specs here stay byte-for-byte stable; most NEW
//! languages are added via the generic tags adapter ([`super::tags_adapter`]):
//! a Cargo dep plus a one-line registry entry, no query authoring. See SUPPORTED.md.

use std::collections::HashMap;
use std::sync::OnceLock;

use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Language, Node as TsNode, Parser, Point, Query, QueryCursor, Tree};

use super::graph::{Node, Origin};

// Compiled queries per language, cached for the process lifetime.
// tree_sitter::Query is Send + Sync (upstream unsafe impl), so LazyLock/OnceLock are safe.
struct CachedQueries {
    defs: Query,
    calls: Query,
    imports: Query,
}

static RUST_QUERIES: OnceLock<CachedQueries> = OnceLock::new();
static PYTHON_QUERIES: OnceLock<CachedQueries> = OnceLock::new();
static JS_QUERIES: OnceLock<CachedQueries> = OnceLock::new();
static TS_QUERIES: OnceLock<CachedQueries> = OnceLock::new();
static GO_QUERIES: OnceLock<CachedQueries> = OnceLock::new();
static SWIFT_QUERIES: OnceLock<CachedQueries> = OnceLock::new();

fn cached_queries(spec: &LangSpec) -> Option<&'static CachedQueries> {
    let slot: &OnceLock<CachedQueries> = match spec.name {
        "rust" => &RUST_QUERIES,
        "python" => &PYTHON_QUERIES,
        "javascript" => &JS_QUERIES,
        "typescript" => &TS_QUERIES,
        "go" => &GO_QUERIES,
        "swift" => &SWIFT_QUERIES,
        _ => return None,
    };
    Some(slot.get_or_init(|| {
        let lang = (spec.language)();
        CachedQueries {
            defs: Query::new(&lang, spec.defs_query).expect("defs query"),
            calls: Query::new(&lang, spec.calls_query).expect("calls query"),
            imports: Query::new(&lang, spec.imports_query).expect("imports query"),
        }
    }))
}

/// Description of how to extract one language.
pub struct LangSpec {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub language: fn() -> Language,
    /// Definition query; each capture name is used as the symbol `kind`.
    pub defs_query: &'static str,
    /// Call query; the single capture marks the callee name node.
    pub calls_query: &'static str,
    /// Import query; the capture marks the import statement node.
    pub imports_query: &'static str,
}

/// The syntactic shape of a markdown cross-doc link, which decides how it resolves:
/// `Inline` and `Reference` targets are file paths resolved relative to the linking
/// file (universally safe, standard CommonMark); `Wikilink` and `Embed` targets are
/// bare note names resolved by basename (PKM, gated on the literal `[[…]]`/`![[…]]`
/// syntax). Reference-style links (`[t][label]`, `[label][]`, `[label]`) resolve
/// against their in-file `[label]: url` definition and are emitted as `Inline` (a
/// resolved reference is a plain path target once the label is substituted), so
/// `Reference` itself is never constructed. `Embed` is constructed for a transclusion
/// (`![[note]]`) and resolves like a `Wikilink` but to an `embeds` edge rather than
/// `imports`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdLinkKind {
    Inline,
    Reference,
    Wikilink,
    Embed,
}

/// One markdown cross-doc link, kept OFF the shared `imports` list so it resolves by
/// its own syntax rules (see [`MdLinkKind`]). `target` is the raw path (inline) or
/// bare note name (wikilink) with any `#anchor` split into `anchor`; the anchor is
/// resolved to a specific heading node in a later task, not here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MdLink {
    /// Inline: the raw link path minus any anchor (`./deploy.md`). Wikilink: the bare
    /// note name (`arch`).
    pub target: String,
    /// The `#anchor` fragment, if the link carried one.
    pub anchor: Option<String>,
    pub kind: MdLinkKind,
    /// 1-based line the link appears on.
    pub line: usize,
}

/// Raw, unresolved extraction for one file.
#[derive(Clone)]
pub struct FileExtract {
    pub module: Node,
    pub defs: Vec<Node>,
    /// (caller_node_id, callee_name, call_site_line): the 1-based line of the
    /// call expression itself, carried into [`Edge::line`](super::graph::Edge::line)
    /// at assembly so callers/callees traversals can cite the exact call site.
    pub calls: Vec<(String, String, usize)>,
    /// (path_segment, line)
    pub imports: Vec<(String, usize)>,
    /// (module_id, def_id) containment
    pub contains: Vec<(String, String)>,
    /// Markdown cross-doc links (empty for every non-markdown language). Resolved by
    /// [`super::resolve_md_links`], not the shared `imports` path.
    pub md_links: Vec<MdLink>,
    /// Frontmatter `tags:` (empty for every non-markdown language). Assembly turns
    /// each distinct tag into a shared `kind:"tag"` node with a `tagged` edge from
    /// this module.
    pub md_tags: Vec<String>,
    /// Frontmatter `aliases:` (empty for every non-markdown language). Extra
    /// name keys under which a `Wikilink`/reference can resolve to this module.
    pub md_aliases: Vec<String>,
}

/// Return the spec for a file extension, if supported.
pub fn spec_for_extension(ext: &str) -> Option<LangSpec> {
    all_specs()
        .into_iter()
        .find(|s| s.extensions.contains(&ext))
}

/// Return the spec for a language name, if supported.
pub fn spec_for_language(name: &str) -> Option<LangSpec> {
    let lname = name.to_ascii_lowercase();
    all_specs().into_iter().find(|s| s.name == lname)
}

/// All supported language specs.
pub fn all_specs() -> Vec<LangSpec> {
    vec![
        LangSpec {
            name: "rust",
            extensions: &["rs"],
            language: || tree_sitter_rust::LANGUAGE.into(),
            defs_query: r#"
                (function_item name: (identifier) @function)
                (function_signature_item name: (identifier) @function_signature)
                (struct_item name: (type_identifier) @struct)
                (enum_item name: (type_identifier) @enum)
                (trait_item name: (type_identifier) @trait)
                (mod_item name: (identifier) @mod)
                (const_item name: (identifier) @const)
                (type_item name: (type_identifier) @type)
            "#,
            calls_query: r#"
                (call_expression function: (identifier) @call)
                (call_expression function: (scoped_identifier name: (identifier) @call))
                (call_expression function: (field_expression field: (field_identifier) @call))
                (call_expression function: (generic_function function: (identifier) @call))
                (call_expression function: (generic_function function: (scoped_identifier name: (identifier) @call)))
                (call_expression function: (generic_function function: (field_expression field: (field_identifier) @call)))
                (macro_invocation macro: (identifier) @call)
            "#,
            imports_query: r#"(use_declaration) @import"#,
        },
        LangSpec {
            name: "python",
            extensions: &["py"],
            language: || tree_sitter_python::LANGUAGE.into(),
            defs_query: r#"
                (function_definition name: (identifier) @function)
                (class_definition name: (identifier) @class)
            "#,
            calls_query: r#"
                (call function: (identifier) @call)
                (call function: (attribute attribute: (identifier) @call))
            "#,
            imports_query: r#"
                (import_statement) @import
                (import_from_statement) @import
            "#,
        },
        LangSpec {
            name: "javascript",
            extensions: &["js", "jsx", "mjs", "cjs"],
            language: || tree_sitter_javascript::LANGUAGE.into(),
            defs_query: r#"
                (function_declaration name: (identifier) @function)
                (method_definition name: (property_identifier) @method)
                (class_declaration name: (identifier) @class)
                (variable_declarator name: (identifier) @function value: (arrow_function))
            "#,
            calls_query: r#"
                (call_expression function: (identifier) @call)
                (call_expression function: (member_expression property: (property_identifier) @call))
            "#,
            imports_query: r#"(import_statement) @import"#,
        },
        LangSpec {
            name: "typescript",
            extensions: &["ts", "tsx"],
            language: || tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            defs_query: r#"
                (function_declaration name: (identifier) @function)
                (method_definition name: (property_identifier) @method)
                (class_declaration name: (type_identifier) @class)
                (interface_declaration name: (type_identifier) @interface)
                (variable_declarator name: (identifier) @function value: (arrow_function))
            "#,
            calls_query: r#"
                (call_expression function: (identifier) @call)
                (call_expression function: (member_expression property: (property_identifier) @call))
            "#,
            imports_query: r#"(import_statement) @import"#,
        },
        LangSpec {
            name: "go",
            extensions: &["go"],
            language: || tree_sitter_go::LANGUAGE.into(),
            defs_query: r#"
                (function_declaration name: (identifier) @function)
                (method_declaration name: (field_identifier) @method)
                (type_spec name: (type_identifier) @struct type: (struct_type))
                (type_spec name: (type_identifier) @interface type: (interface_type))
            "#,
            calls_query: r#"
                (call_expression function: (identifier) @call)
                (call_expression function: (selector_expression field: (field_identifier) @call))
            "#,
            imports_query: r#"(import_declaration) @import"#,
        },
        LangSpec {
            name: "swift",
            extensions: &["swift"],
            language: || tree_sitter_swift::LANGUAGE.into(),
            // tree-sitter-swift parses class/struct/enum all as `class_declaration`
            // (the keyword is an anonymous token we can't capture), so they share
            // the `class` kind. `function_declaration` carries two `name:` fields
            // (the symbol name and the return type); constraining the capture to
            // `(simple_identifier)` picks the name, not the `user_type` return.
            defs_query: r#"
                (function_declaration name: (simple_identifier) @function)
                (protocol_function_declaration name: (simple_identifier) @function)
                (class_declaration name: (type_identifier) @class)
                (protocol_declaration name: (type_identifier) @protocol)
            "#,
            calls_query: r#"
                (call_expression (simple_identifier) @call)
                (call_expression (navigation_expression suffix: (navigation_suffix suffix: (simple_identifier) @call)))
            "#,
            imports_query: r#"(import_declaration) @import"#,
        },
        LangSpec {
            name: "markdown",
            extensions: &["md", "markdown"],
            language: || tree_sitter_md::LANGUAGE.into(),
            // Markdown is extracted by a CUSTOM path ([`extract_markdown`]), not the
            // generic defs/calls/imports query loop: heading names must strip the
            // `#` markers and `contains` must reflect section nesting, neither of
            // which a flat capture query expresses. These queries are never
            // compiled (the early branch in `extract_from_tree` returns first), so
            // they stay empty and markdown needs no `cached_queries` entry.
            defs_query: "",
            calls_query: "",
            imports_query: "",
        },
    ]
}

/// AST node kinds that delimit a callable scope, per language. Used to attribute
/// a call to its enclosing definition.
fn fn_scope_kinds(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["function_item"],
        "python" => &["function_definition"],
        "javascript" => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
            "variable_declarator",
        ],
        "typescript" => &[
            "function_declaration",
            "method_definition",
            "arrow_function",
            "function_expression",
            "variable_declarator",
        ],
        "go" => &["function_declaration", "method_declaration"],
        "swift" => &["function_declaration", "protocol_function_declaration"],
        _ => &[],
    }
}

/// Parse `source` and extract symbols and relationships. Returns `None` if the
/// grammar can't be loaded or the source fails to parse into a tree.
pub fn extract_file(path: &str, source: &str, spec: &LangSpec) -> Option<FileExtract> {
    extract_file_with_tree(path, source, spec).map(|(fx, _tree)| fx)
}

/// Like [`extract_file`] but also returns the parsed `Tree` so callers can cache
/// it and feed it back into [`reparse_incremental`] on the next edit. Parses from
/// scratch (no prior tree).
pub fn extract_file_with_tree(
    path: &str,
    source: &str,
    spec: &LangSpec,
) -> Option<(FileExtract, Tree)> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let fx = extract_from_tree(path, source, spec, &tree)?;
    Some((fx, tree))
}

/// Incrementally re-parse a changed file: edit `old_tree` to match the byte delta
/// between `old_source` and `new_source`, then re-parse `new_source` reusing the
/// old tree (tree-sitter only re-walks the changed regions). Returns the fresh
/// extract and the new tree.
///
/// Byte-identity safety: the returned tree is a complete, correct parse of
/// `new_source` regardless of how precise the [`InputEdit`] was. The edit is only
/// a performance hint for what to re-scan; an imprecise hint costs extra re-scan,
/// never a wrong tree. Extraction reads only the returned tree over `new_source`,
/// so the resulting [`FileExtract`] equals a from-scratch [`extract_file`].
pub fn reparse_incremental(
    path: &str,
    old_source: &str,
    new_source: &str,
    mut old_tree: Tree,
    spec: &LangSpec,
) -> Option<(FileExtract, Tree)> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let edit = input_edit(old_source, new_source);
    old_tree.edit(&edit);
    let tree = parser.parse(new_source, Some(&old_tree))?;
    let fx = extract_from_tree(path, new_source, spec, &tree)?;
    Some((fx, tree))
}

/// Describe the change from `old` to `new` as a single contiguous replacement
/// (the span between the common prefix and the common suffix). Exact for a
/// localized edit; for scattered edits it spans a larger region, which is still
/// correct (tree-sitter just re-scans more). Byte offsets and row/column Points
/// are both filled in, as `Tree::edit` requires.
pub(super) fn input_edit(old: &str, new: &str) -> InputEdit {
    let ob = old.as_bytes();
    let nb = new.as_bytes();

    // Common prefix length (in bytes), clamped to a char boundary of both.
    let max_pre = ob.len().min(nb.len());
    let mut start = 0;
    while start < max_pre && ob[start] == nb[start] {
        start += 1;
    }
    while start > 0 && (!old.is_char_boundary(start) || !new.is_char_boundary(start)) {
        start -= 1;
    }

    // Common suffix length (in bytes), not overlapping the prefix in either.
    let mut suf = 0;
    let old_max_suf = ob.len() - start;
    let new_max_suf = nb.len() - start;
    let max_suf = old_max_suf.min(new_max_suf);
    while suf < max_suf && ob[ob.len() - 1 - suf] == nb[nb.len() - 1 - suf] {
        suf += 1;
    }
    let mut old_end = ob.len() - suf;
    let mut new_end = nb.len() - suf;
    while old_end < ob.len()
        && new_end < nb.len()
        && (!old.is_char_boundary(old_end) || !new.is_char_boundary(new_end))
    {
        old_end += 1;
        new_end += 1;
    }

    InputEdit {
        start_byte: start,
        old_end_byte: old_end,
        new_end_byte: new_end,
        start_position: byte_to_point(old, start),
        old_end_position: byte_to_point(old, old_end),
        new_end_position: byte_to_point(new, new_end),
    }
}

/// Row/column ([`Point`]) of a byte offset within `s` (0-based row, byte column
/// within the row, matching tree-sitter's convention).
fn byte_to_point(s: &str, byte: usize) -> Point {
    let upto = &s.as_bytes()[..byte];
    let row = upto.iter().filter(|&&b| b == b'\n').count();
    let col = match upto.iter().rposition(|&b| b == b'\n') {
        Some(nl) => byte - nl - 1,
        None => byte,
    };
    Point::new(row, col)
}

/// Byte spans (`[start, end)`) of every Rust item governed by a `#[cfg(test)]`
/// gate: for each such attribute, the FULL span of the item it decorates, so a
/// `#[cfg(test)] mod tests { ... }` contributes the whole module brace-range and
/// every nested def falls inside it. Real brace-range detection off the parse
/// tree, NOT a "line-cut to EOF" heuristic: production code after the test module
/// is outside the span. Iterative DFS; spans may nest/overlap, which
/// [`byte_in_spans`] handles.
pub(crate) fn collect_cfg_test_spans(root: &TsNode, src: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut stack = vec![*root];
    while let Some(node) = stack.pop() {
        if node.kind() == "attribute_item" && is_cfg_test_attr(&node, src) {
            if let Some(gov) = governed_item(&node, src) {
                spans.push((gov.start_byte(), gov.end_byte()));
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    spans
}

/// The item a `#[cfg(test)]` attribute governs. In tree-sitter-rust an outer
/// `attribute_item` is a SIBLING of the item it decorates (both children of
/// `source_file`/`declaration_list`), so the governed item is the next named
/// sibling, skipping any stacked attributes. An inner `#![cfg(test)]` instead
/// governs its enclosing block/module/file (its parent).
fn governed_item<'a>(attr: &TsNode<'a>, src: &[u8]) -> Option<TsNode<'a>> {
    if node_text(attr, src).trim_start().starts_with("#![") {
        return attr.parent();
    }
    let mut sib = attr.next_named_sibling();
    while let Some(s) = sib {
        if s.kind() == "attribute_item" {
            sib = s.next_named_sibling();
        } else {
            return Some(s);
        }
    }
    None
}

/// Whether an `attribute_item`'s text is a `#[cfg(test)]`-style compile gate:
/// a `cfg(...)` (or inner `#![cfg(...)]`) whose predicate mentions `test` and is
/// not negated. Deliberately conservative — `cfg_attr(...)` (conditional attribute
/// application, not a compile gate) and any `not(...)` (e.g. `cfg(not(test))`,
/// which is PRODUCTION code) are excluded, so prod code is never mis-marked test.
fn is_cfg_test_attr(attr_item: &TsNode, src: &[u8]) -> bool {
    let text: String = node_text(attr_item, src)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let is_cfg = text.starts_with("#[cfg(") || text.starts_with("#![cfg(");
    is_cfg && text.contains("test") && !text.contains("not(")
}

/// Whether `byte` falls inside any `[start, end)` span.
pub(crate) fn byte_in_spans(byte: usize, spans: &[(usize, usize)]) -> bool {
    spans.iter().any(|(s, e)| byte >= *s && byte < *e)
}

/// Extract symbols and relationships from an already-parsed `tree` over `source`.
/// Shared by the from-scratch and incremental parse paths so both produce an
/// identical [`FileExtract`] for identical source.
fn extract_from_tree(
    path: &str,
    source: &str,
    spec: &LangSpec,
    tree: &Tree,
) -> Option<FileExtract> {
    // Markdown has no query-driven defs/calls/imports; it uses a bespoke walk that
    // turns headings into symbols and section nesting into `contains` edges.
    if spec.name == "markdown" {
        return Some(extract_markdown(path, source, spec, tree));
    }

    let root = tree.root_node();
    let src = source.as_bytes();

    let module = Node::new(path, "module", path, 1, spec.name);
    let mut defs: Vec<Node> = Vec::new();
    let mut contains: Vec<(String, String)> = Vec::new();
    // Map: AST scope node id -> graph node id (function-like defs only).
    let mut scope_map: HashMap<usize, String> = HashMap::new();

    // Use process-wide cached queries (compiled once per language per process).
    let queries = cached_queries(spec)?;

    // Byte spans governed by a `#[cfg(test)]` gate (Rust only), so a def inside one
    // is marked test-origin and discounted for importance. Empty for other langs.
    let cfg_test_spans = if spec.name == "rust" {
        collect_cfg_test_spans(&root, src)
    } else {
        Vec::new()
    };

    // --- definitions ---
    let defs_q = &queries.defs;
    let capture_names = defs_q.capture_names();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(defs_q, root, src);
    while let Some(m) = it.next() {
        for cap in m.captures {
            let kind = capture_names[cap.index as usize];
            let name_node = cap.node;
            let name = node_text(&name_node, src);
            let line = name_node.start_position().row + 1;
            let mut node = Node::new(path, kind, &name, line, spec.name);
            if byte_in_spans(name_node.start_byte(), &cfg_test_spans) {
                node.origin = Origin::Test;
            }
            let nid = node.id.clone();
            contains.push((module.id.clone(), nid.clone()));
            // Record callable scope (the definition node wrapping the name).
            if matches!(kind, "function" | "method") {
                if let Some(scope) = name_node.parent() {
                    scope_map.insert(scope.id(), nid.clone());
                }
            }
            defs.push(node);
        }
    }

    // --- calls ---
    let mut calls: Vec<(String, String, usize)> = Vec::new();
    let scope_kinds = fn_scope_kinds(spec.name);
    let calls_q = &queries.calls;
    let mut ccur = QueryCursor::new();
    let mut cit = ccur.matches(calls_q, root, src);
    while let Some(m) = cit.next() {
        for cap in m.captures {
            let callee = last_segment(&node_text(&cap.node, src));
            if callee.is_empty() {
                continue;
            }
            let line = cap.node.start_position().row + 1;
            let caller = enclosing_scope(&cap.node, &scope_map, scope_kinds)
                .unwrap_or_else(|| module.id.clone());
            calls.push((caller, callee, line));
        }
    }

    // --- imports ---
    let mut imports: Vec<(String, usize)> = Vec::new();
    let imp_q = &queries.imports;
    let mut icur = QueryCursor::new();
    let mut iit = icur.matches(imp_q, root, src);
    while let Some(m) = iit.next() {
        for cap in m.captures {
            let line = cap.node.start_position().row + 1;
            let text = node_text(&cap.node, src);
            for seg in import_targets(&text) {
                imports.push((seg, line));
            }
        }
    }

    Some(FileExtract {
        module,
        defs,
        calls,
        imports,
        contains,
        // Only markdown carries cross-doc links, tags, and aliases; every code
        // language leaves these empty so the byte-identity guarantee holds.
        md_links: Vec::new(),
        md_tags: Vec::new(),
        md_aliases: Vec::new(),
    })
}

/// Custom extraction for markdown (tree-sitter-md block grammar). The grammar
/// nests `section` nodes — each `section` opens with one heading and directly
/// contains the sections of deeper headings — so this walks that nesting to emit
/// one `heading` symbol per heading and a `contains` edge from each heading to the
/// heading of every section nested one level beneath it (a top-level heading is
/// contained by the file's `module` node, matching the other languages). Cross-doc
/// links are collected separately into `md_links`; markdown carries no calls and
/// nothing on the shared `imports` list.
fn extract_markdown(path: &str, source: &str, spec: &LangSpec, tree: &Tree) -> FileExtract {
    let src = source.as_bytes();
    // A leading `--- … ---` frontmatter block, if present, renames the module to its
    // `title:` (falling back to the rel path) and contributes tag/alias keys. Parsed
    // straight off the raw source: the block grammar treats `---` as a thematic
    // break / setext underline, so the tree is not a reliable frontmatter source.
    let fm = parse_frontmatter(source);
    let module_name = fm.title.clone().unwrap_or_else(|| path.to_string());
    let module = Node::new(path, "module", &module_name, 1, spec.name);
    let mut defs: Vec<Node> = Vec::new();
    let mut contains: Vec<(String, String)> = Vec::new();
    let mut md_links: Vec<MdLink> = Vec::new();

    let root = tree.root_node();
    for child in root.children(&mut root.walk()) {
        if child.kind() == "section" {
            walk_md_section(child, &module.id, path, src, spec.name, &mut defs, &mut contains);
        }
    }
    let mut link_defs: HashMap<String, String> = HashMap::new();
    collect_link_defs(root, src, &mut link_defs);
    collect_md_links(root, src, &link_defs, &mut md_links);

    // Frontmatter tags come first; inline `#tag` hashtags in prose are appended
    // (deduped against them and each other) so both feed the same tag-node path.
    let mut md_tags = fm.tags;
    collect_inline_tags(root, src, &mut md_tags);

    FileExtract {
        module,
        defs,
        calls: Vec::new(),
        // Markdown links live on `md_links`, resolved by their own syntax rules; the
        // shared `imports` list stays empty so code-import resolution never sees them.
        imports: Vec::new(),
        contains,
        md_links,
        md_tags,
        md_aliases: fm.aliases,
    }
}

/// Recurse one markdown `section`. Create its heading symbol (its first heading
/// child), link `parent -> this heading` as `contains`, then recurse into every
/// nested `section`, hanging their headings under this one. A leading "prelude"
/// section (content before the first heading, so no heading child) has no symbol
/// of its own; its nested sections attach directly to `parent`.
fn walk_md_section(
    section: TsNode,
    parent: &str,
    path: &str,
    src: &[u8],
    lang: &str,
    defs: &mut Vec<Node>,
    contains: &mut Vec<(String, String)>,
) {
    let heading = section
        .children(&mut section.walk())
        .find(|c| is_md_heading(c.kind()));

    let this_id = match heading {
        Some(h) => {
            let name = md_heading_name(&h, src);
            let line = h.start_position().row + 1;
            let node = Node::new(path, "heading", &name, line, lang);
            let id = node.id.clone();
            contains.push((parent.to_string(), id.clone()));
            defs.push(node);
            id
        }
        None => parent.to_string(),
    };

    for child in section.children(&mut section.walk()) {
        if child.kind() == "section" {
            walk_md_section(child, &this_id, path, src, lang, defs, contains);
        }
    }
}

fn is_md_heading(kind: &str) -> bool {
    matches!(kind, "atx_heading" | "setext_heading")
}

/// Heading text with the `#` markers and surrounding whitespace removed. The block
/// grammar exposes the text after the marker as a `heading_content` field; falling
/// back to the raw heading text (markers stripped) covers headings the grammar
/// leaves without that field.
fn md_heading_name(heading: &TsNode, src: &[u8]) -> String {
    if let Some(content) = heading.child_by_field_name("heading_content") {
        let text = node_text(&content, src).trim().to_string();
        if !text.is_empty() {
            return text;
        }
    }
    node_text(heading, src)
        .trim()
        .trim_start_matches('#')
        .trim()
        .to_string()
}

/// Walk the block tree collecting link-reference definitions (`[label]: url
/// "title"`), keyed by case-folded label so [`scan_md_links`] can resolve
/// full/collapsed/shortcut reference links against them. The block grammar parses
/// each definition as its own `link_reference_definition` node (a sibling of
/// `paragraph`/`heading`, not inline), so this is a separate tree walk from
/// [`collect_md_links`]; definitions can appear anywhere in the file, so the whole
/// tree is scanned before any reference is resolved.
fn collect_link_defs(node: TsNode, src: &[u8], defs: &mut HashMap<String, String>) {
    if matches!(node.kind(), "fenced_code_block" | "indented_code_block") {
        return;
    }
    if node.kind() == "link_reference_definition" {
        let label = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "link_label")
            .map(|c| node_text(&c, src));
        let dest = node
            .children(&mut node.walk())
            .find(|c| c.kind() == "link_destination")
            .map(|c| node_text(&c, src));
        if let (Some(label), Some(dest)) = (label, dest) {
            let label = label.trim().trim_start_matches('[').trim_end_matches(']').trim();
            let dest = dest.trim();
            let dest = dest
                .strip_prefix('<')
                .and_then(|s| s.strip_suffix('>'))
                .unwrap_or(dest);
            if !label.is_empty() {
                defs.entry(label.to_ascii_lowercase())
                    .or_insert_with(|| dest.to_string());
            }
        }
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_link_defs(child, src, defs);
    }
}

/// Walk the block tree collecting cross-doc links from prose. The block grammar
/// leaves inline syntax unparsed, so link markup lives verbatim inside opaque
/// `inline` nodes; code blocks carry no `inline` node and are skipped outright, so
/// links inside code fences never resolve. `defs` is the label -> url map from
/// [`collect_link_defs`], used to resolve reference-style links.
fn collect_md_links(
    node: TsNode,
    src: &[u8],
    defs: &HashMap<String, String>,
    md_links: &mut Vec<MdLink>,
) {
    if matches!(node.kind(), "fenced_code_block" | "indented_code_block") {
        return;
    }
    if node.kind() == "inline" {
        let text = node_text(&node, src);
        scan_md_links(&text, node.start_position().row, defs, md_links);
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_md_links(child, src, defs, md_links);
    }
}

/// Walk the block tree collecting inline `#tag` hashtags from prose, appending each
/// new tag name (sans `#`) to `tags`. Mirrors [`collect_md_links`]'s walk: code
/// blocks carry no `inline` node and are skipped outright, so a `#` inside a fence
/// never becomes a tag (inline code spans are skipped inside [`scan_inline_tags`]).
fn collect_inline_tags(node: TsNode, src: &[u8], tags: &mut Vec<String>) {
    if matches!(node.kind(), "fenced_code_block" | "indented_code_block") {
        return;
    }
    if node.kind() == "inline" {
        scan_inline_tags(&node_text(&node, src), tags);
        return;
    }
    for child in node.children(&mut node.walk()) {
        collect_inline_tags(child, src, tags);
    }
}

/// Scan one prose fragment for inline `#tag` hashtags, appending each new tag name to
/// `tags`. STRICT syntax gating keeps a plain note untouched: a `#` fires ONLY when it
/// is not inside a backtick code span, is not preceded by a word char (so `x.md#sec`
/// stays a link anchor) nor a `(` (so `](#sec)` stays a link destination), and is
/// followed by a run of tag chars (letters/digits/`-`/`_`/`/`). ATX heading markers
/// are separate tokens outside the `inline` text, so they are never seen here. Dedups
/// against the tags already collected (frontmatter plus earlier inline).
fn scan_inline_tags(text: &str, tags: &mut Vec<String>) {
    let is_tag_char = |c: char| c.is_alphanumeric() || matches!(c, '-' | '_' | '/');
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut in_code = false;
    let mut prev: Option<char> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((idx, ch)) = chars.next() {
        if ch == '`' {
            in_code = !in_code;
            prev = Some('`');
            continue;
        }
        let gated =
            in_code || ch != '#' || matches!(prev, Some(p) if is_word(p) || p == '(');
        if gated {
            prev = Some(ch);
            continue;
        }
        // `#` is ASCII (one byte); the tag-char run follows it immediately.
        let start = idx + 1;
        let mut end = start;
        while let Some(&(_, c)) = chars.peek() {
            if is_tag_char(c) {
                end += c.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        if end > start {
            let tag = &text[start..end];
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.to_string());
            }
            prev = tag.chars().last();
        } else {
            prev = Some('#');
        }
    }
}

/// Scan one prose fragment for the markdown link shapes, emitting each as an
/// [`MdLink`]. Standard `[text](target)` becomes an [`MdLinkKind::Inline`] link
/// (raw path kept, so it can later be resolved relative to the linking file); the
/// Obsidian `[[target]]`/`[[target#anchor]]` wikilink becomes an
/// [`MdLinkKind::Wikilink`] link (bare note name); and the three reference forms —
/// full `[text][label]`, collapsed `[label][]`, shortcut `[label]` — resolve
/// against `defs` (see [`push_ref_link`]). All are handled in a single left-to-right
/// pass. `base_row` is the fragment's 0-based start row, used to attribute each
/// link to its own line.
fn scan_md_links(text: &str, base_row: usize, defs: &HashMap<String, String>, md_links: &mut Vec<MdLink>) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        // A `!` immediately before the bracket marks a transclusion embed
        // (`![[note]]`) or a Markdown image (`![alt](url)`); the leading `!` is the
        // ONLY thing separating an embed from a plain wikilink, so it must be checked
        // here, before the wikilink branch.
        let embed = i > 0 && bytes[i - 1] == b'!';
        let rest = &text[i..];
        // Obsidian wikilink `[[target]]` / transclusion embed `![[target]]` (a
        // wikilink with a leading `!`). Consuming the whole `[[…]]` here is what keeps
        // an embed's inner `[[note]]` from also being scanned as a separate wikilink.
        if let Some(inner) = rest.strip_prefix("[[") {
            if let Some(end) = inner.find("]]") {
                let kind = if embed {
                    MdLinkKind::Embed
                } else {
                    MdLinkKind::Wikilink
                };
                push_md_link(kind, &inner[..end], text, i, base_row, md_links);
                i += 2 + end + 2;
                continue;
            }
        }
        // A Markdown image `![alt](url)` (a single-bracket `![…]`, not `![[`) is
        // neither a cross-doc link nor an embed: step past this bracket so its `(url)`
        // is never mistaken for an inline link target.
        if embed {
            i += 1;
            continue;
        }
        // The link text ends at the first `]`; what follows decides the shape:
        // `(target)` -> inline, `[label]` -> full/collapsed reference, otherwise a
        // bare shortcut reference candidate.
        if let Some(rb) = rest.find(']') {
            if rest[rb + 1..].starts_with('(') {
                if let Some(rp) = rest[rb + 2..].find(')') {
                    push_md_link(
                        MdLinkKind::Inline,
                        &rest[rb + 2..rb + 2 + rp],
                        text,
                        i,
                        base_row,
                        md_links,
                    );
                    i += rb + 2 + rp + 1;
                    continue;
                }
            } else if rest[rb + 1..].starts_with('[') {
                // Full `[text][label]` / collapsed `[text][]` reference.
                if let Some(rb2) = rest[rb + 2..].find(']') {
                    let label_span = &rest[rb + 2..rb + 2 + rb2];
                    let label = if label_span.trim().is_empty() {
                        &rest[1..rb] // collapsed: label is the link text itself
                    } else {
                        label_span
                    };
                    push_ref_link(label, defs, text, i, base_row, md_links);
                    i += rb + 2 + rb2 + 1;
                    continue;
                }
            } else {
                // Shortcut reference candidate: [label] with nothing following.
                push_ref_link(&rest[1..rb], defs, text, i, base_row, md_links);
                i += rb + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Turn one raw link `target` of the given `kind` into an [`MdLink`], attributed to
/// the link's line (`base_row` plus newlines before the link within `text`). Inline
/// targets keep their raw path (only the `#anchor` is split off) and skip external
/// `http(s)://` URLs; wikilink and embed targets are the bare note name with the
/// anchor split off. Resolving the anchor to a specific heading is a later task's job.
fn push_md_link(
    kind: MdLinkKind,
    raw: &str,
    text: &str,
    at: usize,
    base_row: usize,
    md_links: &mut Vec<MdLink>,
) {
    let line = base_row + text[..at].matches('\n').count() + 1;
    let raw = raw.trim();
    match kind {
        MdLinkKind::Inline => {
            let lower = raw.to_ascii_lowercase();
            if lower.starts_with("http://") || lower.starts_with("https://") {
                return; // external URL, not a cross-doc link
            }
            let (target, anchor) = split_md_anchor(raw);
            md_links.push(MdLink { target, anchor, kind: MdLinkKind::Inline, line });
        }
        // Wikilink `[[note]]` and embed `![[note]]` share the same bare-name target;
        // only the `kind` (and, later, the edge kind it resolves to) differs.
        MdLinkKind::Wikilink | MdLinkKind::Embed => {
            let (target, anchor) = split_md_anchor(raw);
            md_links.push(MdLink { target, anchor, kind, line });
        }
        // Reference resolves via push_ref_link (emitted as Inline, not this kind) and
        // never reaches this match arm.
        MdLinkKind::Reference => {}
    }
}

/// Resolve one reference-form link (full `[text][label]`, collapsed `[label][]`, or
/// shortcut `[label]`) against the in-file `[label]: url` definitions collected by
/// [`collect_link_defs`] (label matching is case-insensitive, per CommonMark). A
/// label with no matching definition contributes no link — CommonMark treats the
/// unresolved brackets as literal text, not an error. A match is emitted as an
/// [`MdLinkKind::Inline`] link: once the label is substituted for its definition's
/// URL, a resolved reference link is just a path target, same as a standard inline
/// link.
fn push_ref_link(
    label: &str,
    defs: &HashMap<String, String>,
    text: &str,
    at: usize,
    base_row: usize,
    md_links: &mut Vec<MdLink>,
) {
    let Some(dest) = defs.get(&label.trim().to_ascii_lowercase()) else {
        return;
    };
    let line = base_row + text[..at].matches('\n').count() + 1;
    let (target, anchor) = split_md_anchor(dest);
    md_links.push(MdLink { target, anchor, kind: MdLinkKind::Inline, line });
}

/// Split a link target on its first `#` into `(path-or-name, anchor)`, trimming both.
/// An empty or missing fragment yields `None` for the anchor. `arch#Overview` ->
/// `("arch", Some("Overview"))`; `./deploy.md` -> `("./deploy.md", None)`.
fn split_md_anchor(target: &str) -> (String, Option<String>) {
    match target.split_once('#') {
        Some((before, after)) => {
            let after = after.trim();
            let anchor = if after.is_empty() {
                None
            } else {
                Some(after.to_string())
            };
            (before.trim().to_string(), anchor)
        }
        None => (target.trim().to_string(), None),
    }
}

/// The subset of YAML frontmatter lens cares about: the display `title`, plus the
/// `tags` and `aliases` lists. Everything else in the block is ignored.
#[derive(Default)]
struct Frontmatter {
    title: Option<String>,
    tags: Vec<String>,
    aliases: Vec<String>,
}

/// Which list key a block-style (`- item`) continuation belongs to.
#[derive(Clone, Copy)]
enum FmListKey {
    Tags,
    Aliases,
}

/// Parse a leading YAML frontmatter block into [`Frontmatter`]. A tiny hand-rolled
/// scanner, NOT a general YAML parser: it recognizes the block ONLY when the file's
/// very first line is a `---` fence and a closing `---`/`...` fence follows, then
/// pulls `title:` (a scalar) and the `tags:`/`aliases:` lists in all three shapes —
/// flow (`[a, b]`), block (`- a` / `- b` on their own lines), and a bare
/// space/comma-separated list. Anything without that exact leading-fence shape (a
/// plain document, a mid-file `---` rule) yields an empty result, so a non-PKM note
/// is untouched. Markdown-only (its sole caller is [`extract_markdown`]).
fn parse_frontmatter(source: &str) -> Frontmatter {
    let mut fm = Frontmatter::default();
    let mut lines = source.lines();
    // The opening fence must be the very first line of the file.
    match lines.next() {
        Some(first) if first.trim() == "---" => {}
        _ => return fm,
    }
    // Collect the block body up to the closing fence. No closing fence ⇒ the leading
    // `---` was not frontmatter (e.g. a document opening on a thematic break).
    let mut body: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            closed = true;
            break;
        }
        body.push(line);
    }
    if !closed {
        return Frontmatter::default();
    }

    let mut current: Option<FmListKey> = None;
    for line in body {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // A block-style list item continues the most recent `tags:`/`aliases:` key.
        if let Some(item) = block_item(trimmed) {
            let cleaned = clean_scalar(item);
            if let (Some(key), false) = (current, cleaned.is_empty()) {
                match key {
                    FmListKey::Tags => fm.tags.push(cleaned),
                    FmListKey::Aliases => fm.aliases.push(cleaned),
                }
            }
            continue;
        }
        // Only unindented `key: value` lines are recognized; an indented non-list
        // line belongs to a nested mapping we do not model.
        if line.starts_with(char::is_whitespace) {
            continue;
        }
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        current = None;
        match key.trim() {
            "title" => {
                let title = clean_scalar(val);
                if !title.is_empty() {
                    fm.title = Some(title);
                }
            }
            "tags" => {
                fm.tags.extend(parse_yaml_list(val));
                if val.is_empty() {
                    current = Some(FmListKey::Tags);
                }
            }
            "aliases" => {
                fm.aliases.extend(parse_yaml_list(val));
                if val.is_empty() {
                    current = Some(FmListKey::Aliases);
                }
            }
            _ => {}
        }
    }
    fm
}

/// The value of a block-style list item (`- foo` -> `foo`, bare `-` -> ``), or
/// `None` if the (already-trimmed) line is not a list item.
fn block_item(trimmed: &str) -> Option<&str> {
    if let Some(rest) = trimmed.strip_prefix("- ") {
        Some(rest)
    } else if trimmed == "-" {
        Some("")
    } else {
        None
    }
}

/// Parse a `tags:`/`aliases:` value that shares a line with its key. A flow list
/// (`[a, b]`) and a bare comma-list split on commas (so a quoted item may hold
/// spaces); a bare space-list splits on whitespace. An empty value means a block
/// list follows on subsequent lines, so this returns nothing.
fn parse_yaml_list(val: &str) -> Vec<String> {
    let val = val.trim();
    if val.is_empty() {
        return Vec::new();
    }
    if let Some(inner) = val.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner
            .split(',')
            .map(clean_scalar)
            .filter(|s| !s.is_empty())
            .collect();
    }
    let by_comma = val.contains(',');
    val.split(|c: char| if by_comma { c == ',' } else { c.is_whitespace() })
        .map(clean_scalar)
        .filter(|s| !s.is_empty())
        .collect()
}

/// Trim a YAML scalar and strip one layer of matching single/double quotes.
fn clean_scalar(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

pub(super) fn node_text(node: &TsNode, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// Walk up from `node` to the nearest enclosing callable scope present in
/// `scope_map`, returning its graph node id.
pub(super) fn enclosing_scope(
    node: &TsNode,
    scope_map: &HashMap<usize, String>,
    scope_kinds: &[&str],
) -> Option<String> {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if scope_kinds.contains(&n.kind()) {
            if let Some(id) = scope_map.get(&n.id()) {
                return Some(id.clone());
            }
        }
        cur = n.parent();
    }
    None
}

/// Last identifier segment of a (possibly qualified) call name.
pub(super) fn last_segment(name: &str) -> String {
    name.rsplit(['.', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(name)
        .trim()
        .to_string()
}

/// Names a (possibly grouped) import statement brings into scope. A braced group
/// (`use a::{b, c}`, `import { b, c }`) yields one name per member; everything
/// else yields the single representative target [`import_target`] picks. This is
/// what makes a multi-symbol `use` emit one import edge per symbol, not just the
/// last token.
pub(super) fn import_targets(stmt: &str) -> Vec<String> {
    if let (Some(open), Some(close)) = (stmt.find('{'), stmt.rfind('}')) {
        if close > open {
            let names: Vec<String> = stmt[open + 1..close]
                .split(',')
                .filter_map(import_target)
                .collect();
            if !names.is_empty() {
                return names;
            }
        }
    }
    import_target(stmt).into_iter().collect()
}

/// Pull a representative target name out of an import statement's text.
fn import_target(stmt: &str) -> Option<String> {
    // Collect identifier-ish tokens, ignoring keywords/punctuation, and return
    // the last one (the imported symbol/module in most languages).
    let keywords = [
        "use", "pub", "import", "from", "as", "crate", "self", "super", "mod", "require",
    ];
    let mut tokens: Vec<String> = Vec::new();
    let mut cur = String::new();
    for ch in stmt.chars() {
        if ch.is_alphanumeric() || ch == '_' {
            cur.push(ch);
        } else {
            if !cur.is_empty() {
                tokens.push(std::mem::take(&mut cur));
            }
        }
    }
    if !cur.is_empty() {
        tokens.push(cur);
    }
    tokens
        .into_iter()
        .rfind(|t| !keywords.contains(&t.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(defs: &[Node]) -> Vec<String> {
        let mut k: Vec<String> = defs.iter().map(|n| n.kind.clone()).collect();
        k.sort();
        k.dedup();
        k
    }

    #[test]
    fn swift_extraction() {
        let src = r#"
import Foundation

protocol Greeter { func greet() -> String }

struct Person: Greeter {
    let name: String
    func greet() -> String { return makeGreeting(name) }
}

class Widget {
    func render() {
        let p = Person(name: "x")
        print(p.greet())
    }
}

func makeGreeting(_ n: String) -> String { "hi" }
"#;
        let spec = spec_for_language("swift").unwrap();
        let fx = extract_file("a.swift", src, &spec).unwrap();

        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"function".to_string()), "kinds: {ks:?}");
        assert!(ks.contains(&"class".to_string()), "kinds: {ks:?}"); // struct Person
        assert!(ks.contains(&"protocol".to_string()), "kinds: {ks:?}");

        let names: Vec<&str> = fx.defs.iter().map(|n| n.name.as_str()).collect();
        for want in ["greet", "makeGreeting", "Person", "Widget", "Greeter"] {
            assert!(names.contains(&want), "missing def {want}; got {names:?}");
        }

        let callees: Vec<&str> = fx.calls.iter().map(|(_, c, _)| c.as_str()).collect();
        assert!(callees.contains(&"makeGreeting"), "callees: {callees:?}"); // greet() body
        assert!(callees.contains(&"greet"), "callees: {callees:?}"); // p.greet()

        assert!(
            fx.imports.iter().any(|(s, _)| s == "Foundation"),
            "imports: {:?}",
            fx.imports
        );
    }

    #[test]
    fn rust_extraction() {
        let src = r#"
use std::fs::read;

struct Widget { x: i32 }

fn helper() -> i32 { 42 }

fn main() {
    let w = helper();
    println!("{}", w);
}
"#;
        let spec = spec_for_language("rust").unwrap();
        let fx = extract_file("a.rs", src, &spec).unwrap();
        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"function".to_string()));
        assert!(ks.contains(&"struct".to_string()));
        // main calls helper, and the call SITE line (line 9: `let w = helper();`)
        // is captured — the witness evidence transitive_closure surfaces.
        assert!(fx.calls.iter().any(|(_, c, l)| c == "helper" && *l == 9));
        // imported read
        assert!(fx.imports.iter().any(|(p, _)| p == "read"));
    }

    #[test]
    fn rust_cfg_test_span_marks_only_test_origin() {
        // Brace-range detection, NOT a line-cut heuristic: `after` sits BELOW the
        // `#[cfg(test)]` module yet stays prod; only defs inside the module's
        // brace-range are `Test`. `cfg(not(test))` is production, never test.
        let src = r#"
fn before() -> i32 { 1 }

#[cfg(test)]
mod tests {
    fn inside_test() -> i32 { 2 }
}

fn after() -> i32 { 3 }

#[cfg(not(test))]
fn prod_only() -> i32 { 4 }
"#;
        let spec = spec_for_language("rust").unwrap();
        let fx = extract_file("a.rs", src, &spec).unwrap();
        let origin_of = |name: &str| {
            fx.defs
                .iter()
                .find(|d| d.name == name)
                .unwrap_or_else(|| panic!("missing def {name}"))
                .origin
        };
        assert_eq!(origin_of("before"), Origin::Prod);
        assert_eq!(origin_of("inside_test"), Origin::Test);
        assert_eq!(
            origin_of("after"),
            Origin::Prod,
            "def after the test mod must stay prod (brace-range, not line-cut)"
        );
        assert_eq!(
            origin_of("prod_only"),
            Origin::Prod,
            "cfg(not(test)) is production code"
        );
    }

    #[test]
    fn rust_turbofish_calls() {
        let src = r#"
fn helper<T>() -> T { unimplemented!() }

fn plain() -> i32 { 1 }

fn main() {
    let a = helper::<i32>();
    let s = "5".parse::<i32>();
    let v = Vec::<u8>::new();
    let w = plain();
}
"#;
        let spec = spec_for_language("rust").unwrap();
        let fx = extract_file("a.rs", src, &spec).unwrap();
        let callees: Vec<&str> = fx.calls.iter().map(|(_, c, _)| c.as_str()).collect();
        assert!(callees.contains(&"helper"), "helper::<i32>() missing; callees: {callees:?}");
        assert!(callees.contains(&"parse"), ".parse::<i32>() missing; callees: {callees:?}");
        assert!(callees.contains(&"new"), "Vec::<u8>::new() missing; callees: {callees:?}");
        assert!(callees.contains(&"plain"), "plain control call missing; callees: {callees:?}");
    }

    #[test]
    fn python_extraction() {
        let src = r#"
import os

class Foo:
    def method(self):
        return helper()

def helper():
    return 1
"#;
        let spec = spec_for_language("python").unwrap();
        let fx = extract_file("a.py", src, &spec).unwrap();
        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"function".to_string()));
        assert!(ks.contains(&"class".to_string()));
        assert!(fx.calls.iter().any(|(_, c, _)| c == "helper"));
        assert!(fx.imports.iter().any(|(p, _)| p == "os"));
    }

    #[test]
    fn javascript_extraction() {
        let src = r#"
import { thing } from './mod';

class Foo {
  bar() { return helper(); }
}

function helper() { return 1; }
const arrow = () => helper();
"#;
        let spec = spec_for_language("javascript").unwrap();
        let fx = extract_file("a.js", src, &spec).unwrap();
        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"function".to_string()));
        assert!(ks.contains(&"class".to_string()));
        assert!(ks.contains(&"method".to_string()));
        assert!(fx.calls.iter().any(|(_, c, _)| c == "helper"));
        assert!(fx.imports.iter().any(|(p, _)| p == "thing" || p == "mod"));
    }

    #[test]
    fn typescript_extraction() {
        let src = r#"
import { X } from './x';

interface Shape { area(): number; }

class Circle implements Shape {
  area(): number { return compute(); }
}

function compute(): number { return 1; }
"#;
        let spec = spec_for_language("typescript").unwrap();
        let fx = extract_file("a.ts", src, &spec).unwrap();
        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"interface".to_string()));
        assert!(ks.contains(&"class".to_string()));
        assert!(ks.contains(&"method".to_string()));
        assert!(fx.calls.iter().any(|(_, c, _)| c == "compute"));
    }

    #[test]
    fn go_extraction() {
        let src = r#"
package main

import "fmt"

type Widget struct { x int }

func helper() int { return 1 }

func main() {
    v := helper()
    fmt.Println(v)
}
"#;
        let spec = spec_for_language("go").unwrap();
        let fx = extract_file("a.go", src, &spec).unwrap();
        let ks = kinds(&fx.defs);
        assert!(ks.contains(&"function".to_string()));
        assert!(ks.contains(&"struct".to_string()));
        assert!(fx.calls.iter().any(|(_, c, _)| c == "helper"));
        assert!(fx.imports.iter().any(|(p, _)| p == "fmt"));
    }

    #[test]
    fn extension_dispatch() {
        assert_eq!(spec_for_extension("rs").unwrap().name, "rust");
        assert_eq!(spec_for_extension("py").unwrap().name, "python");
        assert!(spec_for_extension("xyz").is_none());
    }

    #[test]
    fn markdown_extraction() {
        // Nested headings A > B > C plus a sibling D under A. The block grammar
        // nests `section` nodes, so containment must mirror heading depth:
        // A⊃B, B⊃C, A⊃D — and crucially NOT A⊃C (C is nested under B).
        const MD: &str = "\
# A
intro prose under A

## B
body of B

### C
deep body of C

## D
sibling body of D
";
        // Both extensions must resolve to the markdown spec.
        assert_eq!(spec_for_extension("md").unwrap().name, "markdown");
        assert_eq!(spec_for_extension("markdown").unwrap().name, "markdown");

        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("doc.md", MD, &spec).unwrap();

        // Every def is a heading; names are exactly the marker-stripped text.
        assert_eq!(kinds(&fx.defs), ["heading".to_string()], "only heading kind");
        let mut names: Vec<&str> = fx.defs.iter().map(|n| n.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["A", "B", "C", "D"], "heading names");

        let id = |name: &str| -> String {
            fx.defs
                .iter()
                .find(|n| n.name == name)
                .unwrap_or_else(|| panic!("no heading {name}; got {names:?}"))
                .id
                .clone()
        };
        let has = |from: &str, to: &str| {
            fx.contains.iter().any(|(f, t)| *f == id(from) && *t == id(to))
        };

        assert!(has("A", "B"), "A⊃B missing; contains={:?}", fx.contains);
        assert!(has("B", "C"), "B⊃C missing; contains={:?}", fx.contains);
        assert!(has("A", "D"), "A⊃D missing; contains={:?}", fx.contains);
        // Section nesting is one level deep, not transitive.
        assert!(!has("A", "C"), "A must NOT directly contain C; contains={:?}", fx.contains);
    }

    #[test]
    fn markdown_links() {
        // A standard link, an anchored wikilink, an external link, and a link
        // buried in a fenced code block (which must NOT resolve).
        const MD: &str = "\
# Title

See [deploy](./deploy.md) for rollout and [[arch#Overview]] for design.
External [site](https://example.com) is not a note.

```text
[incode](./secret.md)
```
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("index.md", MD, &spec).unwrap();

        // Markdown links land on `md_links`, never the shared `imports` list.
        assert!(
            fx.imports.is_empty(),
            "markdown must not populate the shared imports; got {:?}",
            fx.imports
        );

        // Inline `[deploy](./deploy.md)`: the raw path is kept (no anchor here).
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Inline
                && l.target == "./deploy.md"
                && l.anchor.is_none()),
            "inline ./deploy.md link missing; got {:?}",
            fx.md_links
        );
        // Wikilink `[[arch#Overview]]`: bare note name plus its split-off anchor.
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Wikilink
                && l.target == "arch"
                && l.anchor.as_deref() == Some("Overview")),
            "wikilink arch#Overview missing; got {:?}",
            fx.md_links
        );

        // The external https link contributes no md_link.
        assert!(
            !fx.md_links
                .iter()
                .any(|l| l.target.contains("example") || l.target.contains("http")),
            "external link must not produce an md_link; got {:?}",
            fx.md_links
        );
        // A link inside a fenced code block is not a cross-doc link.
        assert!(
            !fx.md_links.iter().any(|l| l.target.contains("secret")),
            "code-fence link must be ignored; got {:?}",
            fx.md_links
        );
    }

    #[test]
    fn markdown_reference_links() {
        // Full, collapsed, and shortcut reference forms, all resolving to the same
        // def, plus a label with no matching def (which must drop silently).
        const MD: &str = "\
# Title

See [t][d] for rollout, then [d][] again, and bare [d] once more.
A [missing] reference has no def.

[d]: ./deploy.md \"Deploy\"
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("index.md", MD, &spec).unwrap();

        let resolved = |l: &MdLink| {
            l.kind == MdLinkKind::Inline && l.target == "./deploy.md" && l.anchor.is_none()
        };
        let count = fx.md_links.iter().filter(|l| resolved(l)).count();
        assert_eq!(
            count, 3,
            "full/collapsed/shortcut references must all resolve to ./deploy.md; got {:?}",
            fx.md_links
        );

        // A label with no matching definition contributes no MdLink.
        assert!(
            !fx.md_links.iter().any(|l| l.target.contains("missing")),
            "unresolved reference must not produce an md_link; got {:?}",
            fx.md_links
        );
    }

    #[test]
    fn markdown_embeds() {
        // A transclusion embed, an anchored embed, a plain wikilink (must stay a
        // Wikilink, not be swallowed or re-emitted by the embed), and a Markdown image
        // (which is NEITHER a link nor an embed).
        const MD: &str = "\
# Notes

Transclude ![[deploy]] and ![[arch#Overview]] inline.
Also a plain [[arch]] wikilink and an image ![diagram](./pic.png).
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("notes.md", MD, &spec).unwrap();

        // `![[deploy]]` -> an Embed with the bare note name, no anchor.
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Embed
                && l.target == "deploy"
                && l.anchor.is_none()),
            "embed ![[deploy]] missing; got {:?}",
            fx.md_links
        );
        // `![[arch#Overview]]` -> an Embed carrying its split-off anchor.
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Embed
                && l.target == "arch"
                && l.anchor.as_deref() == Some("Overview")),
            "anchored embed ![[arch#Overview]] missing; got {:?}",
            fx.md_links
        );
        // The plain `[[arch]]` stays a Wikilink (the embed detection must not
        // reclassify or double-count it).
        assert_eq!(
            fx.md_links
                .iter()
                .filter(|l| l.kind == MdLinkKind::Wikilink)
                .count(),
            1,
            "exactly one Wikilink expected (no double-count from embeds); got {:?}",
            fx.md_links
        );
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Wikilink
                && l.target == "arch"
                && l.anchor.is_none()),
            "plain [[arch]] wikilink missing; got {:?}",
            fx.md_links
        );
        // The embed's inner `[[deploy]]`/`[[arch#Overview]]` was consumed by the
        // embed, so it is never ALSO a Wikilink.
        assert!(
            !fx.md_links
                .iter()
                .any(|l| l.kind == MdLinkKind::Wikilink && l.target == "deploy"),
            "embed inner [[deploy]] must not also be a wikilink; got {:?}",
            fx.md_links
        );
        // A Markdown image contributes no link of any kind.
        assert!(
            !fx.md_links.iter().any(|l| l.target.contains("pic")),
            "image ![diagram](./pic.png) must not produce an md_link; got {:?}",
            fx.md_links
        );
    }

    #[test]
    fn markdown_inline_tags() {
        // Inline hashtags in prose, a hierarchical tag, a duplicate (dedup), a code
        // span (skip), a heading marker (never a tag), and two link/anchor forms that
        // must NOT be mistaken for tags.
        const MD: &str = "\
# Heading

Some #alpha findings and #beta considerations, plus #alpha again and a #area/sub tag.
Ignore `#incode` inside a code span, the link [x](arch.md#section) and [y](#local).
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("notes.md", MD, &spec).unwrap();

        // Real prose tags, deduped, in document order.
        assert_eq!(
            fx.md_tags,
            vec!["alpha", "beta", "area/sub"],
            "inline #tags (deduped, hierarchical kept); got {:?}",
            fx.md_tags
        );
        // None of the excluded `#` forms leaked in.
        for bad in ["incode", "section", "local", "Heading"] {
            assert!(
                !fx.md_tags.iter().any(|t| t == bad),
                "`{bad}` must not be a tag; got {:?}",
                fx.md_tags
            );
        }
    }

    /// Extract-level half of the T6 no-regression gate: a plain CommonMark note (an
    /// inline link + headings, but NO `[[`, NO `![[`, NO `#tag`) yields ZERO
    /// `Wikilink`/`Embed` links and ZERO tags — the PKM name-based machinery never
    /// engages. (The graph-edge half is asserted in the `discovery::mod` tests.)
    #[test]
    fn markdown_plain_note_has_no_pkm_extraction() {
        const MD: &str = "\
# Plain CommonMark

A normal inline link to [architecture](./arch.md) for reference.

## Content

Regular prose with no special hashtag syntax for tags.

## Structure

- Item one
- Item two
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("plain.md", MD, &spec).unwrap();

        assert!(
            !fx.md_links
                .iter()
                .any(|l| matches!(l.kind, MdLinkKind::Wikilink | MdLinkKind::Embed)),
            "plain note must yield no Wikilink/Embed links; got {:?}",
            fx.md_links
        );
        assert!(
            fx.md_tags.is_empty(),
            "plain note must yield no tags; got {:?}",
            fx.md_tags
        );
        // The ordinary inline link is still extracted (this is standard CommonMark,
        // owned by lens; only the PKM syntaxes are gated off).
        assert!(
            fx.md_links.iter().any(|l| l.kind == MdLinkKind::Inline
                && l.target == "./arch.md"),
            "the plain inline link must still be extracted; got {:?}",
            fx.md_links
        );
    }

    #[test]
    fn markdown_frontmatter() {
        // Flow-list tags, block-list aliases, a quoted title, and body headings that
        // must still extract normally alongside the frontmatter.
        const MD: &str = "\
---
title: My Note
tags: [alpha, beta]
aliases:
  - oldname
  - \"Legacy Name\"
---
# Body

## Section
";
        let spec = spec_for_language("markdown").unwrap();
        let fx = extract_file("note.md", MD, &spec).unwrap();

        // `title:` renames the module; its `file` stays the rel path.
        assert_eq!(fx.module.name, "My Note", "title must rename the module node");
        assert_eq!(fx.module.file, "note.md", "module file must stay the rel path");
        assert_eq!(fx.md_tags, vec!["alpha", "beta"], "flow-list tags");
        assert_eq!(
            fx.md_aliases,
            vec!["oldname", "Legacy Name"],
            "block-list aliases (quoted multi-word preserved)"
        );
        // Body headings are unaffected by the frontmatter block.
        assert!(
            fx.defs.iter().any(|n| n.kind == "heading" && n.name == "Body"),
            "body heading still extracted; defs={:?}",
            fx.defs
        );
    }

    #[test]
    fn markdown_frontmatter_list_forms_and_absence() {
        let spec = spec_for_language("markdown").unwrap();

        // Bare comma list and a bare space list resolve to the same tokens.
        let comma = extract_file("a.md", "---\ntags: alpha, beta\n---\n# X\n", &spec).unwrap();
        assert_eq!(comma.md_tags, vec!["alpha", "beta"], "bare comma list");
        let space = extract_file("b.md", "---\ntags: alpha beta\n---\n# X\n", &spec).unwrap();
        assert_eq!(space.md_tags, vec!["alpha", "beta"], "bare space list");

        // No frontmatter: module keeps its path name, tags/aliases stay empty.
        let plain = extract_file("c.md", "# Just a heading\n\nProse.\n", &spec).unwrap();
        assert_eq!(plain.module.name, "c.md", "no title ⇒ module named by path");
        assert!(plain.md_tags.is_empty() && plain.md_aliases.is_empty());

        // A leading `---` with no closing fence is a thematic break, not frontmatter.
        let rule = extract_file("d.md", "---\n# Heading after a rule\n", &spec).unwrap();
        assert_eq!(rule.module.name, "d.md", "unterminated `---` is not frontmatter");
        assert!(rule.md_tags.is_empty() && rule.md_aliases.is_empty());
    }

    /// Compare two extracts on every field, in order, so an incremental reparse can
    /// be proven equal to a from-scratch parse.
    fn assert_extract_eq(a: &FileExtract, b: &FileExtract) {
        assert_eq!(a.module, b.module, "module node differs");
        assert_eq!(a.defs, b.defs, "defs differ");
        assert_eq!(a.calls, b.calls, "calls differ");
        assert_eq!(a.imports, b.imports, "imports differ");
        assert_eq!(a.contains, b.contains, "contains differ");
        assert_eq!(a.md_links, b.md_links, "md_links differ");
        assert_eq!(a.md_tags, b.md_tags, "md_tags differ");
        assert_eq!(a.md_aliases, b.md_aliases, "md_aliases differ");
    }

    /// The core per-file guarantee behind byte-identity: an incremental reparse
    /// (edit old tree, reparse reusing it) yields an extract identical to a
    /// from-scratch parse of the new source. Exercised across edit shapes:
    /// in-place rename, insertion, deletion, and a no-op.
    #[test]
    fn incremental_reparse_matches_fresh() {
        let spec = spec_for_language("rust").unwrap();
        let old = "fn helper() -> i32 { 42 }\nfn main() { let _ = helper(); }\n";
        let cases = [
            // rename a callee (changes ids, calls resolution endpoints)
            "fn helper2() -> i32 { 42 }\nfn main() { let _ = helper2(); }\n",
            // insert a new function in the middle
            "fn helper() -> i32 { 42 }\nfn extra() {}\nfn main() { let _ = helper(); }\n",
            // delete the body / shrink
            "fn helper() -> i32 { 0 }\nfn main() {}\n",
            // prepend an import (shifts every byte/line below)
            "use std::fs;\nfn helper() -> i32 { 42 }\nfn main() { let _ = helper(); }\n",
            // no-op edit (identical text)
            "fn helper() -> i32 { 42 }\nfn main() { let _ = helper(); }\n",
        ];

        for new in cases {
            let (_, base_tree) = extract_file_with_tree("a.rs", old, &spec).unwrap();
            let (inc_fx, _) =
                reparse_incremental("a.rs", old, new, base_tree, &spec).unwrap();
            let fresh_fx = extract_file("a.rs", new, &spec).unwrap();
            assert_extract_eq(&inc_fx, &fresh_fx);
        }
    }

    /// Multi-byte UTF-8 in the changed region must not break the InputEdit boundary
    /// math (the parse must still match a fresh parse).
    #[test]
    fn incremental_reparse_handles_unicode() {
        let spec = spec_for_language("rust").unwrap();
        let old = "fn greet() { let s = \"hi\"; }\n";
        let new = "fn greet() { let s = \"héllo wörld 🌍\"; }\n";
        let (_, base_tree) = extract_file_with_tree("u.rs", old, &spec).unwrap();
        let (inc_fx, _) = reparse_incremental("u.rs", old, new, base_tree, &spec).unwrap();
        let fresh_fx = extract_file("u.rs", new, &spec).unwrap();
        assert_extract_eq(&inc_fx, &fresh_fx);
    }
}
