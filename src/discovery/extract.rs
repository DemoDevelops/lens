//! tree-sitter parsing: source file -> symbols + raw relationships.
//!
//! Each language is described by a [`LangSpec`]: the grammar plus three queries
//! (definitions, calls, imports) and the AST node kinds that count as a callable
//! scope. Adding a language is a single `LangSpec` entry (see DECISIONS.md).

use std::collections::HashMap;

use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node as TsNode, Parser, Query, QueryCursor};

use super::graph::Node;

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
                (struct_item name: (type_identifier) @struct)
                (enum_item name: (type_identifier) @enum)
                (trait_item name: (type_identifier) @trait)
                (mod_item name: (identifier) @mod)
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
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    let src = source.as_bytes();

    let module = Node::new(path, "module", path, 1, spec.name);
    let mut defs: Vec<Node> = Vec::new();
    let mut contains: Vec<(String, String)> = Vec::new();
    // Map: AST scope node id -> graph node id (function-like defs only).
    let mut scope_map: HashMap<usize, String> = HashMap::new();

    // --- definitions ---
    let defs_q = Query::new(&language, spec.defs_query).ok()?;
    let capture_names = defs_q.capture_names();
    let mut cursor = QueryCursor::new();
    let mut it = cursor.matches(&defs_q, root, src);
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
    if let Ok(calls_q) = Query::new(&language, spec.calls_query) {
        let mut ccur = QueryCursor::new();
        let mut cit = ccur.matches(&calls_q, root, src);
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
    }

    // --- imports ---
    let mut imports: Vec<(String, usize)> = Vec::new();
    if let Ok(imp_q) = Query::new(&language, spec.imports_query) {
        let mut icur = QueryCursor::new();
        let mut iit = icur.matches(&imp_q, root, src);
        while let Some(m) = iit.next() {
            for cap in m.captures {
                let line = cap.node.start_position().row + 1;
                let text = node_text(&cap.node, src);
                if let Some(seg) = import_target(&text) {
                    imports.push((seg, line));
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

fn node_text(node: &TsNode, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

/// Walk up from `node` to the nearest enclosing callable scope present in
/// `scope_map`, returning its graph node id.
fn enclosing_scope(
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
fn last_segment(name: &str) -> String {
    name.rsplit(['.', ':'])
        .find(|s| !s.is_empty())
        .unwrap_or(name)
        .trim()
        .to_string()
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
}
