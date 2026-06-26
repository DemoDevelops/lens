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

use super::graph::Node;

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

/// Raw, unresolved extraction for one file.
#[derive(Clone)]
pub struct FileExtract {
    pub module: Node,
    pub defs: Vec<Node>,
    /// (caller_node_id, callee_name)
    pub calls: Vec<(String, String)>,
    /// (path_segment, line)
    pub imports: Vec<(String, usize)>,
    /// (module_id, def_id) containment
    pub contains: Vec<(String, String)>,
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

/// Extract symbols and relationships from an already-parsed `tree` over `source`.
/// Shared by the from-scratch and incremental parse paths so both produce an
/// identical [`FileExtract`] for identical source.
fn extract_from_tree(
    path: &str,
    source: &str,
    spec: &LangSpec,
    tree: &Tree,
) -> Option<FileExtract> {
    let root = tree.root_node();
    let src = source.as_bytes();

    let module = Node::new(path, "module", path, 1, spec.name);
    let mut defs: Vec<Node> = Vec::new();
    let mut contains: Vec<(String, String)> = Vec::new();
    // Map: AST scope node id -> graph node id (function-like defs only).
    let mut scope_map: HashMap<usize, String> = HashMap::new();

    // Use process-wide cached queries (compiled once per language per process).
    let queries = cached_queries(spec)?;

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
            let node = Node::new(path, kind, &name, line, spec.name);
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
    let mut calls: Vec<(String, String)> = Vec::new();
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
            let caller = enclosing_scope(&cap.node, &scope_map, scope_kinds)
                .unwrap_or_else(|| module.id.clone());
            calls.push((caller, callee));
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
    })
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

        let callees: Vec<&str> = fx.calls.iter().map(|(_, c)| c.as_str()).collect();
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
        // main calls helper
        assert!(fx.calls.iter().any(|(_, c)| c == "helper"));
        // imported read
        assert!(fx.imports.iter().any(|(p, _)| p == "read"));
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
        assert!(fx.calls.iter().any(|(_, c)| c == "helper"));
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
        assert!(fx.calls.iter().any(|(_, c)| c == "helper"));
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
        assert!(fx.calls.iter().any(|(_, c)| c == "compute"));
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
        assert!(fx.calls.iter().any(|(_, c)| c == "helper"));
        assert!(fx.imports.iter().any(|(p, _)| p == "fmt"));
    }

    #[test]
    fn extension_dispatch() {
        assert_eq!(spec_for_extension("rs").unwrap().name, "rust");
        assert_eq!(spec_for_extension("py").unwrap().name, "python");
        assert!(spec_for_extension("xyz").is_none());
    }

    /// Compare two extracts on every field, in order, so an incremental reparse can
    /// be proven equal to a from-scratch parse.
    fn assert_extract_eq(a: &FileExtract, b: &FileExtract) {
        assert_eq!(a.module, b.module, "module node differs");
        assert_eq!(a.defs, b.defs, "defs differ");
        assert_eq!(a.calls, b.calls, "calls differ");
        assert_eq!(a.imports, b.imports, "imports differ");
        assert_eq!(a.contains, b.contains, "contains differ");
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
