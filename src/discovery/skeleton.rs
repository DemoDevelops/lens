//! File-skeleton view (prototype, internal-only): emit each definition's
//! signature + nesting with executable bodies elided to a single `…`.
//!
//! This reuses the existing tree-sitter parse (same `LangSpec` grammars as
//! [`super::extract`]). It is deterministic: it walks the AST in source order
//! and emits byte ranges verbatim, so the output ordering is fixed by the file.
//!
//! Not registered as an MCP tool (lens invariant 5). It exists as an internal
//! function plus a standalone measurement test.

use tree_sitter::{Node as TsNode, Parser};

use super::extract::LangSpec;

/// The character that replaces an elided body.
const ELLIPSIS: char = '…';

/// Container definition kinds whose body holds *other* definitions we want to
/// keep (so we recurse into them) rather than executable statements we elide.
/// Function/method bodies are not containers: their body is elided wholesale.
fn container_kinds(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["mod_item", "impl_item", "trait_item"],
        "python" => &["class_definition"],
        "javascript" => &["class_declaration", "class_body"],
        "typescript" => &[
            "class_declaration",
            "class_body",
            "interface_declaration",
            "interface_body",
        ],
        "go" => &[],
        "swift" => &["class_declaration", "protocol_declaration", "class_body"],
        _ => &[],
    }
}

/// The AST node kind that holds a definition's body (the part we elide for a
/// leaf def, or recurse into for a container). Checked by kind name so we stay
/// grammar-generic without enumerating every def shape.
fn is_body_node(kind: &str) -> bool {
    matches!(
        kind,
        "block"                 // rust fn / python suite-as-block (n/a) / generic
            | "declaration_list" // rust impl/trait/mod body
            | "field_declaration_list"
            | "statement_block"  // js/ts fn + class? (class uses class_body)
            | "class_body"
            | "interface_body"
            | "enum_variant_list"
            | "struct_pattern"
    )
}

// Python uses an indentation `block` for suites; handled by "block" above.
// For Python the class/function body node kind is `block`.

/// Produce a skeleton of `source`: signatures and nesting preserved, executable
/// bodies replaced by `…`. Returns `None` if the grammar can't be loaded or the
/// source fails to parse.
pub fn skeletonize(source: &str, spec: &LangSpec) -> Option<String> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let src = source.as_bytes();
    let mut out = String::new();
    let root = tree.root_node();
    emit_children(root, src, spec.name, &mut out);
    // Collapse any run of blank lines introduced by elision to a single newline
    // for stable, compact output.
    Some(normalize_blank_lines(&out))
}

/// Emit the source-order children of `node`, eliding bodies. Top-level entry
/// walks the root's children.
fn emit_children(node: TsNode, src: &[u8], lang: &str, out: &mut String) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        emit_node(child, src, lang, out);
    }
}

/// Emit a single top-level-or-nested node. If it's a container definition we
/// keep its header and recurse into the body; otherwise we keep the header and
/// elide the body to `…`.
fn emit_node(node: TsNode, src: &[u8], lang: &str, out: &mut String) {
    let kind = node.kind();

    // Find the body child (if any) by kind.
    let body = find_body_child(node);

    match body {
        None => {
            // No body: emit the node verbatim (imports, use decls, consts,
            // type aliases, struct field lists we treat as leaf, etc.).
            push_text(node, src, out);
            out.push('\n');
        }
        Some(body) => {
            // Emit the header: everything from node start up to body start.
            push_range(src, node.start_byte(), body.start_byte(), out);
            let is_container = container_kinds(lang).contains(&kind);
            if is_container {
                // Keep the body's opening delimiter, recurse to keep nested
                // signatures, then the closing delimiter.
                emit_container_body(body, src, lang, out);
            } else {
                // Leaf def (function/method): elide the whole body to a single
                // ellipsis, but keep the body's delimiters for readability.
                emit_elided_body(body, src, out);
            }
            // Emit any trailing bytes after the body (e.g. a `;` after a Rust
            // `struct X { ... };` is rare, but Go/TS sometimes have trailers).
            push_range(src, body.end_byte(), node.end_byte(), out);
            out.push('\n');
        }
    }
}

/// Find a child of `node` that is the definition's body, by kind.
fn find_body_child(node: TsNode) -> Option<TsNode> {
    (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|c| is_body_node(c.kind()))
}

/// A container body: keep the delimiter run and recurse into nested defs.
fn emit_container_body(body: TsNode, src: &[u8], lang: &str, out: &mut String) {
    // Opening delimiter: the body's first byte up to its first named child.
    let first_named = first_named_child(body);
    let open_end = first_named
        .map(|c| c.start_byte())
        .unwrap_or(body.end_byte());
    push_range(src, body.start_byte(), open_end, out);
    // Recurse into the body's children, emitting their signatures.
    emit_children(body, src, lang, out);
    // Closing delimiter: from the last named child end to body end.
    let last_named = last_named_child(body);
    let close_start = last_named.map(|c| c.end_byte()).unwrap_or(body.start_byte());
    push_range(src, close_start, body.end_byte(), out);
}

/// A leaf body: keep just the opening + closing delimiter with `…` between.
fn emit_elided_body(body: TsNode, src: &[u8], out: &mut String) {
    let text = node_text(body, src);
    // Brace-delimited body: `{ … }`. Otherwise (e.g. python indented block) emit
    // `…` on its own.
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        out.push_str("{ ");
        out.push(ELLIPSIS);
        out.push_str(" }");
    } else if trimmed.starts_with(':') {
        // Python: header ended with ':'; body is an indented suite.
        out.push(' ');
        out.push(ELLIPSIS);
    } else {
        out.push(ELLIPSIS);
    }
}

fn first_named_child(node: TsNode) -> Option<TsNode> {
    node.named_child(0)
}

fn last_named_child(node: TsNode) -> Option<TsNode> {
    node.named_child_count()
        .checked_sub(1)
        .and_then(|i| node.named_child(i))
}

fn node_text(node: TsNode, src: &[u8]) -> String {
    node.utf8_text(src).unwrap_or("").to_string()
}

fn push_text(node: TsNode, src: &[u8], out: &mut String) {
    out.push_str(node.utf8_text(src).unwrap_or(""));
}

fn push_range(src: &[u8], start: usize, end: usize, out: &mut String) {
    if start >= end || end > src.len() {
        return;
    }
    if let Ok(s) = std::str::from_utf8(&src[start..end]) {
        out.push_str(s);
    }
}

/// Collapse runs of 2+ blank lines (left by body elision) into one. Preserves
/// single blank lines and trims a trailing blank.
fn normalize_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run >= 2 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::extract::spec_for_language;
    use crate::obs::count_tokens;

    /// Real src/ files to measure skeleton compression against. Paths are
    /// relative to the crate root (CARGO_MANIFEST_DIR).
    const SAMPLE_FILES: &[&str] = &[
        "src/discovery/extract.rs",
        "src/discovery/mod.rs",
        "src/discovery/graph.rs",
        "src/discovery/query.rs",
        "src/index/mod.rs",
    ];

    fn read_sample(rel: &str) -> String {
        let root = env!("CARGO_MANIFEST_DIR");
        let path = std::path::Path::new(root).join(rel);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// Every top-level Rust `fn NAME`, `struct NAME`, `enum NAME`, `trait NAME`
    /// signature must survive skeletonization. We check the def keyword+name
    /// pair appears in the skeleton for each top-level def in the original.
    #[test]
    fn top_level_signatures_survive() {
        let spec = spec_for_language("rust").unwrap();
        for rel in SAMPLE_FILES {
            let src = read_sample(rel);
            let skel = skeletonize(&src, &spec).expect("skeletonize");
            // Collect top-level def names from the original via the existing
            // extractor, then assert each name still appears in the skeleton.
            let fx = crate::discovery::extract::extract_file(rel, &src, &spec)
                .expect("extract");
            for def in &fx.defs {
                // Assert only on structural signature-bearing kinds. `const`
                // and `type` are excluded here because Rust allows them as
                // *statement-local* items inside a function body, which a
                // skeleton correctly elides (covered by `top_level_const_and_type_survive`).
                if matches!(
                    def.kind.as_str(),
                    "function" | "struct" | "enum" | "trait" | "mod"
                ) {
                    assert!(
                        skel.contains(&def.name),
                        "{rel}: skeleton dropped def `{}` (kind {})",
                        def.name,
                        def.kind
                    );
                }
            }
        }
    }

    /// Bodies are actually elided: the ellipsis appears and the skeleton is
    /// strictly smaller in tokens than the full file.
    #[test]
    fn skeleton_saves_tokens() {
        let spec = spec_for_language("rust").unwrap();
        let mut full_total = 0usize;
        let mut skel_total = 0usize;
        for rel in SAMPLE_FILES {
            let src = read_sample(rel);
            let skel = skeletonize(&src, &spec).expect("skeletonize");
            let full_t = count_tokens(&src);
            let skel_t = count_tokens(&skel);
            assert!(
                skel.contains(ELLIPSIS),
                "{rel}: no body was elided (no ellipsis present)"
            );
            assert!(
                skel_t < full_t,
                "{rel}: skeleton ({skel_t}) not smaller than full ({full_t})"
            );
            eprintln!(
                "{rel}: full={full_t} skel={skel_t} saved={:.1}%",
                100.0 * (full_t - skel_t) as f64 / full_t as f64
            );
            full_total += full_t;
            skel_total += skel_t;
        }
        let pct = 100.0 * (full_total - skel_total) as f64 / full_total as f64;
        eprintln!(
            "TOTAL: full={full_total} skel={skel_total} saved={pct:.1}%"
        );
        assert!(skel_total < full_total);
    }

    /// Determinism: skeletonizing the same source twice yields identical output.
    #[test]
    fn deterministic() {
        let spec = spec_for_language("rust").unwrap();
        let src = read_sample("src/discovery/extract.rs");
        let a = skeletonize(&src, &spec).unwrap();
        let b = skeletonize(&src, &spec).unwrap();
        assert_eq!(a, b);
    }

    /// Module-level (top-level) const and type aliases survive, even though
    /// function-local ones are elided with the body.
    #[test]
    fn top_level_const_and_type_survive() {
        let spec = spec_for_language("rust").unwrap();
        let src = r#"
const MAX: usize = 10;
type Id = String;

fn work() {
    const LOCAL: usize = 3;
    let _ = LOCAL;
}
"#;
        let skel = skeletonize(src, &spec).unwrap();
        assert!(skel.contains("const MAX"), "top-level const dropped:\n{skel}");
        assert!(skel.contains("type Id"), "top-level type dropped:\n{skel}");
        // The function-local const is correctly elided with the body.
        assert!(!skel.contains("LOCAL"), "fn-local const leaked:\n{skel}");
    }

    /// Nested signatures (impl methods, trait sigs) survive too.
    #[test]
    fn nested_signatures_survive() {
        let spec = spec_for_language("rust").unwrap();
        let src = r#"
struct Widget { x: i32 }

impl Widget {
    fn new() -> Self { let x = compute(); Widget { x } }
    fn render(&self) -> String { format!("{}", self.x) }
}

trait Draw {
    fn draw(&self);
}
"#;
        let skel = skeletonize(src, &spec).unwrap();
        for want in ["struct Widget", "impl Widget", "fn new", "fn render", "trait Draw", "fn draw"] {
            assert!(skel.contains(want), "skeleton dropped `{want}`:\n{skel}");
        }
        // Bodies elided: the compute() call and format!() must be gone.
        assert!(!skel.contains("compute()"), "body not elided:\n{skel}");
        assert!(!skel.contains("format!"), "body not elided:\n{skel}");
        assert!(skel.contains(ELLIPSIS));
    }
}
