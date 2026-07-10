//! File-skeleton view (prototype, internal-only): emit each definition's
//! signature + nesting with executable bodies elided to a single `…`.
//!
//! This reuses the existing tree-sitter parse (same `LangSpec` grammars as
//! [`super::extract`]). It is deterministic: it walks the AST in source order
//! and emits byte ranges verbatim, so the output ordering is fixed by the file.
//!
//! Wrapped by the `lens_skeleton` MCP tool (`src/server.rs`); an optional
//! `include_bodies` list lets a caller name specific definitions whose bodies
//! should be emitted verbatim instead of elided.

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

/// Kinds whose body IS the signature (short, non-executable declarations),
/// so we emit it verbatim rather than eliding or delimiter-recursing.
fn is_signature_body_kind(lang: &str, kind: &str) -> bool {
    lang == "rust" && matches!(kind, "struct_item" | "enum_item" | "union_item")
}

/// Produce a skeleton of `source`: signatures and nesting preserved, executable
/// bodies replaced by `…`. `include_bodies`, if given, names definitions whose
/// bodies should be emitted in full instead of elided; unmatched names are
/// ignored. When `with_lines` is true, every emitted definition's header line
/// is prefixed with its 1-indexed source line number in the exact form
/// `L{n}: ` (e.g. `L12: fn foo() {`), the only annotation format produced,
/// so a caller can parse it back out unambiguously. Only definition
/// header/signature lines get this prefix; lines inside a verbatim body
/// (struct/enum fields, an `include_bodies` body's interior) are never
/// prefixed, and `with_lines: false` reproduces today's output byte-for-byte.
/// Markdown output is unaffected by `with_lines` (headings keep their current
/// form regardless). Returns `None` if the grammar can't be loaded or the
/// source fails to parse.
pub fn skeletonize(
    source: &str,
    spec: &LangSpec,
    include_bodies: Option<&[String]>,
    with_lines: bool,
) -> Option<String> {
    let language = (spec.language)();
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;
    let src = source.as_bytes();
    let mut out = String::new();
    let root = tree.root_node();
    // Markdown has no single "body" node per definition — a `section`'s heading and
    // its prose/subsections are siblings, so the generic container model
    // (`container_kinds`/`is_body_node`/`find_body_child`) doesn't map. Emit the
    // heading tree directly instead, collapsing each section's prose to one `…`.
    if spec.name == "markdown" {
        emit_md_children(root, src, &mut out);
        return Some(normalize_blank_lines(&out));
    }
    emit_children(root, src, spec.name, include_bodies, with_lines, &mut out);
    // Collapse any run of blank lines introduced by elision to a single newline
    // for stable, compact output.
    Some(normalize_blank_lines(&out))
}

/// Walk the `section` children of a markdown `document`/`section` node and emit
/// each as a heading + elided prose (see [`emit_md_section`]). Non-section
/// children (a heading-less document prelude's opaque blocks) are skipped here.
fn emit_md_children(node: TsNode, src: &[u8], out: &mut String) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "section" {
            emit_md_section(child, src, out);
        }
    }
}

/// Emit one markdown `section`: its heading line(s) verbatim (keeping the `#`
/// markers), then a single `…` standing in for the section's own prose blocks
/// (any direct child that is neither the heading nor a nested `section`;
/// consecutive prose collapses to one ellipsis), then its nested sections in
/// source order. A heading-less prelude section emits no heading and no ellipsis
/// of its own — it just hangs its subsections here, matching `extract`.
fn emit_md_section(section: TsNode, src: &[u8], out: &mut String) {
    let mut cursor = section.walk();
    let children: Vec<TsNode> = section.children(&mut cursor).collect();

    if let Some(heading) = children.iter().find(|c| is_md_heading_kind(c.kind())) {
        push_heading_line(*heading, src, out);
        let has_prose = children
            .iter()
            .any(|c| !is_md_heading_kind(c.kind()) && c.kind() != "section");
        if has_prose {
            out.push(ELLIPSIS);
            out.push('\n');
        }
    }

    for child in &children {
        if child.kind() == "section" {
            emit_md_section(*child, src, out);
        }
    }
}

/// The markdown heading node kinds (block grammar). Mirrors `extract::is_md_heading`.
fn is_md_heading_kind(kind: &str) -> bool {
    matches!(kind, "atx_heading" | "setext_heading")
}

/// Emit a heading's source text verbatim, normalized to end in exactly one
/// newline (a setext heading keeps its internal text/underline newline).
fn push_heading_line(heading: TsNode, src: &[u8], out: &mut String) {
    out.push_str(node_text(heading, src).trim_end());
    out.push('\n');
}

/// Emit the source-order children of `node`, eliding bodies. Top-level entry
/// walks the root's children.
fn emit_children(node: TsNode, src: &[u8], lang: &str, include_bodies: Option<&[String]>, with_lines: bool, out: &mut String) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        emit_node(child, src, lang, include_bodies, with_lines, out);
    }
}

/// If `with_lines` is true and `node` is a named node, prefix `out` with its
/// 1-indexed source line in `L{n}: ` form. Gated on `node.is_named()` so the
/// anonymous delimiter tokens (`{`, `,`, `}`) that `emit_container_body`
/// walks never get a stray line-number prefix.
fn push_line_prefix(node: TsNode, with_lines: bool, out: &mut String) {
    if with_lines && node.is_named() {
        out.push_str(&format!("L{}: ", node.start_position().row + 1));
    }
}

/// Emit a single top-level-or-nested node. If it's a container definition we
/// keep its header and recurse into the body; otherwise we keep the header and
/// elide the body to `…`, unless its name is in `include_bodies` or its body
/// is itself a signature (struct/enum/union fields), in which case the body
/// is kept verbatim.
fn emit_node(node: TsNode, src: &[u8], lang: &str, include_bodies: Option<&[String]>, with_lines: bool, out: &mut String) {
    let kind = node.kind();

    // Find the body child (if any) by kind.
    let body = find_body_child(node);

    match body {
        None => {
            // No body: emit the node verbatim (imports, use decls, consts,
            // type aliases, struct field lists we treat as leaf, etc.).
            push_line_prefix(node, with_lines, out);
            push_text(node, src, out);
            out.push('\n');
        }
        Some(body) => {
            // Emit the header: everything from node start up to body start.
            push_line_prefix(node, with_lines, out);
            push_range(src, node.start_byte(), body.start_byte(), out);
            let is_container = container_kinds(lang).contains(&kind);
            if is_container {
                // Keep the body's opening delimiter, recurse to keep nested
                // signatures, then the closing delimiter.
                emit_container_body(body, src, lang, include_bodies, with_lines, out);
            } else if wants_body(node, src, include_bodies) || is_signature_body_kind(lang, kind) {
                // Caller asked for this definition's body verbatim, or the body
                // IS the signature (struct/enum/union fields) rather than
                // executable code, so it's never elided.
                push_range(src, body.start_byte(), body.end_byte(), out);
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

/// True if `node`'s definition name is in `include_bodies`. Name extraction
/// mirrors `extract`'s definitions: the tree-sitter `name` field, falling back
/// to the first `identifier`-kind child.
fn wants_body(node: TsNode, src: &[u8], include_bodies: Option<&[String]>) -> bool {
    let Some(wanted) = include_bodies else {
        return false;
    };
    let Some(name) = def_name(node, src) else {
        return false;
    };
    wanted.iter().any(|w| w == &name)
}

/// Extract a definition node's name: the `name` field if the grammar has one,
/// else the first child whose kind is an identifier variant.
fn def_name(node: TsNode, src: &[u8]) -> Option<String> {
    let name_node = node.child_by_field_name("name").or_else(|| {
        (0..node.child_count())
            .filter_map(|i| node.child(i))
            .find(|c| c.kind().contains("identifier"))
    })?;
    Some(node_text(name_node, src))
}

/// Find a child of `node` that is the definition's body, by kind.
fn find_body_child(node: TsNode) -> Option<TsNode> {
    (0..node.child_count())
        .filter_map(|i| node.child(i))
        .find(|c| is_body_node(c.kind()))
}

/// A container body: keep the delimiter run and recurse into nested defs.
fn emit_container_body(body: TsNode, src: &[u8], lang: &str, include_bodies: Option<&[String]>, with_lines: bool, out: &mut String) {
    // Opening delimiter: the body's first byte up to its first named child.
    let first_named = first_named_child(body);
    let open_end = first_named
        .map(|c| c.start_byte())
        .unwrap_or(body.end_byte());
    push_range(src, body.start_byte(), open_end, out);
    // Recurse into the body's children, emitting their signatures.
    emit_children(body, src, lang, include_bodies, with_lines, out);
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
            let skel = skeletonize(&src, &spec, None, false).expect("skeletonize");
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
            let skel = skeletonize(&src, &spec, None, false).expect("skeletonize");
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
        let a = skeletonize(&src, &spec, None, false).unwrap();
        let b = skeletonize(&src, &spec, None, false).unwrap();
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
        let skel = skeletonize(src, &spec, None, false).unwrap();
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
        let skel = skeletonize(src, &spec, None, false).unwrap();
        for want in ["struct Widget", "impl Widget", "fn new", "fn render", "trait Draw", "fn draw"] {
            assert!(skel.contains(want), "skeleton dropped `{want}`:\n{skel}");
        }
        // The struct's field is part of its signature, not an executable body: it
        // must survive (L49), unlike the fn bodies elided below.
        assert!(skel.contains("x: i32"), "struct field dropped:\n{skel}");
        // Bodies elided: the compute() call and format!() must be gone.
        assert!(!skel.contains("compute()"), "body not elided:\n{skel}");
        assert!(!skel.contains("format!"), "body not elided:\n{skel}");
        assert!(skel.contains(ELLIPSIS));
    }

    /// L49: a struct's fields are its signature, not an executable body, so they
    /// must survive skeletonization instead of being elided like a fn body.
    #[test]
    fn struct_fields_survive() {
        let spec = spec_for_language("rust").unwrap();
        let src = r#"
struct Point {
    x: i32,
    y: i32,
}

fn helper() {
    let _ = 1;
}
"#;
        let skel = skeletonize(src, &spec, None, false).unwrap();
        for field in ["x: i32", "y: i32"] {
            assert!(skel.contains(field), "skeleton dropped field `{field}`:\n{skel}");
        }
        // No ellipsis anywhere in the struct's own definition specifically (the
        // fn body's ellipsis, checked below, is a separate, expected elision).
        let struct_part = skel.split("fn helper").next().unwrap();
        assert!(
            !struct_part.contains(ELLIPSIS),
            "struct body was elided:\n{skel}"
        );
        assert!(
            skel.contains(ELLIPSIS),
            "fn body should still be elided:\n{skel}"
        );
        // The struct body is emitted verbatim (source-faithful formatting, one
        // field per line, trailing comma kept), not delimiter-recursed into
        // stray one-token-per-line junk. Pins the exact source slice.
        assert!(
            skel.contains("struct Point {\n    x: i32,\n    y: i32,\n}"),
            "struct body not emitted verbatim:\n{skel}"
        );
    }

    /// L49: an enum's variants (including a struct-like variant's fields) are
    /// its signature and must survive, not be elided.
    #[test]
    fn enum_variants_survive() {
        let spec = spec_for_language("rust").unwrap();
        let src = r#"
enum Shape {
    Circle(f64),
    Rect { w: f64, h: f64 },
}

fn helper() {
    let _ = 1;
}
"#;
        let skel = skeletonize(src, &spec, None, false).unwrap();
        for want in ["Circle(f64)", "Rect", "w: f64", "h: f64"] {
            assert!(skel.contains(want), "skeleton dropped `{want}`:\n{skel}");
        }
        // No ellipsis anywhere in the enum's own definition specifically (the fn
        // body's ellipsis, checked below, is a separate, expected elision).
        let enum_part = skel.split("fn helper").next().unwrap();
        assert!(
            !enum_part.contains(ELLIPSIS),
            "enum body was elided:\n{skel}"
        );
        assert!(
            skel.contains(ELLIPSIS),
            "fn body should still be elided:\n{skel}"
        );
    }

    const FOO_BAR_SRC: &str = r#"
fn foo() {
    let x = 1;
    println!("{}", x);
}

fn bar() {
    let y = 2;
    println!("{}", y);
}
"#;

    /// `include_bodies` emits the named definition's body verbatim while other
    /// definitions stay elided.
    #[test]
    fn include_bodies_emits_named_body_verbatim() {
        let spec = spec_for_language("rust").unwrap();
        let include = vec!["foo".to_string()];
        let skel = skeletonize(FOO_BAR_SRC, &spec, Some(&include), false).unwrap();
        assert!(
            skel.contains("let x = 1;") && skel.contains("println!(\"{}\", x);"),
            "foo's body not emitted verbatim:\n{skel}"
        );
        assert!(
            !skel.contains("let y = 2;"),
            "bar's body should stay elided:\n{skel}"
        );
        assert!(skel.contains(ELLIPSIS), "bar's elision missing:\n{skel}");
    }

    /// `None` output is unchanged (regression against the pre-change behavior).
    #[test]
    fn none_output_is_golden() {
        let spec = spec_for_language("rust").unwrap();
        let skel = skeletonize(FOO_BAR_SRC, &spec, None, false).unwrap();
        let expected = "fn foo() { … }\nfn bar() { … }\n";
        assert_eq!(skel, expected);
    }

    /// L48: `with_lines: false` reproduces the pre-existing golden output
    /// byte-for-byte, and `with_lines: true` actually changes the output (the
    /// flag has an effect, not just plumbing).
    #[test]
    fn with_lines_false_is_byte_identical_to_none() {
        let spec = spec_for_language("rust").unwrap();
        let expected = "fn foo() { … }\nfn bar() { … }\n";
        let skel_false = skeletonize(FOO_BAR_SRC, &spec, None, false).unwrap();
        assert_eq!(
            skel_false, expected,
            "with_lines=false must match the pre-existing golden output"
        );

        let skel_true = skeletonize(FOO_BAR_SRC, &spec, None, true).unwrap();
        assert_ne!(
            skel_true, skel_false,
            "with_lines=true must change the output"
        );
    }

    /// L48: `with_lines: true` prefixes every definition's header line with
    /// its real 1-indexed source line number (`L{n}: `), across top-level
    /// fns, a struct, an enum, and an impl block with two methods. A
    /// verbatim struct field line must NOT carry a line-number prefix (only
    /// the header does).
    #[test]
    fn with_lines_annotates_every_signature() {
        let spec = spec_for_language("rust").unwrap();
        let src = r#"fn alpha() {
    let _ = 1;
}

struct Point {
    x: i32,
    y: i32,
}

enum Shape {
    Circle(f64),
    Square(f64),
}

impl Point {
    fn new() -> Self {
        Point { x: 0, y: 0 }
    }

    fn sum(&self) -> i32 {
        self.x + self.y
    }
}

fn beta() {
    let _ = 2;
}
"#;
        let skel = skeletonize(src, &spec, None, true).unwrap();

        // Hand cross-checked against the raw fixture text above (1-indexed):
        //   line 1:  `fn alpha() {`               (first line of the raw string)
        //   line 5:  `struct Point {`              (2 fn lines + `}` + blank = line 5)
        //   line 15: `impl Point {`                (after fn alpha, struct Point, enum Shape + blanks)
        for (line, want) in [
            (1, "fn alpha"),
            (5, "struct Point"),
            (10, "enum Shape"),
            (15, "impl Point"),
            (16, "fn new"),
            (20, "fn sum"),
            (25, "fn beta"),
        ] {
            let prefix = format!("L{line}: ");
            let matched = skel
                .lines()
                .any(|l| l.starts_with(&prefix) && l.contains(want));
            assert!(matched, "expected `{prefix}` before `{want}` in:\n{skel}");
        }

        // The struct field is emitted verbatim as part of the signature body
        // (L49), but it is NOT itself a definition header, so it must not
        // carry a line-number prefix.
        let field_line = skel
            .lines()
            .find(|l| l.contains("x: i32,"))
            .expect("struct field line present");
        assert!(
            !field_line.trim_start().starts_with('L'),
            "struct field line should not carry a line-number prefix: `{field_line}`"
        );
    }

    /// Markdown skeletonization keeps the full heading tree and collapses each
    /// section's prose to a single `…`. Exercises the T2 fixture corpus.
    #[test]
    fn markdown_skeleton() {
        let spec = spec_for_language("markdown").expect("markdown spec");
        let src = read_sample("tests/fixtures/md/index.md");
        let skel = skeletonize(&src, &spec, None, false).expect("skeletonize markdown");

        // Every heading LINE from the source survives verbatim (markers kept).
        for heading in ["# Index", "## Setup", "### Local", "## Remote"] {
            assert!(
                skel.contains(heading),
                "skeleton dropped heading `{heading}`:\n{skel}"
            );
        }
        // The ellipsis stands in for elided prose.
        assert!(
            skel.contains(ELLIPSIS),
            "no prose was elided (no ellipsis present):\n{skel}"
        );
        // Paragraph prose is dropped.
        let prose = "This is the main index document linking to other sections.";
        assert!(
            !skel.contains(prose),
            "skeleton leaked paragraph prose:\n{skel}"
        );
        // Strictly fewer tokens than the source (same measure as `skeleton_saves_tokens`).
        let full_t = count_tokens(&src);
        let skel_t = count_tokens(&skel);
        assert!(
            skel_t < full_t,
            "skeleton ({skel_t}) not smaller than source ({full_t}):\n{skel}"
        );
    }

    /// An unknown requested name is ignored: output is identical to `None`.
    #[test]
    fn unknown_include_body_name_is_ignored() {
        let spec = spec_for_language("rust").unwrap();
        let none_skel = skeletonize(FOO_BAR_SRC, &spec, None, false).unwrap();
        let include = vec!["nonexistent".to_string()];
        let unknown_skel = skeletonize(FOO_BAR_SRC, &spec, Some(&include), false).unwrap();
        assert_eq!(unknown_skel, none_skel);
    }
}
