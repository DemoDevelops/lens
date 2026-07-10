//! `$META` pattern compiler for `lens_grep_ast`: turns an ast-grep-style code
//! pattern (`$X.unwrap()`, `print($X)`) into a tree-sitter query, in-binary,
//! using the grammars lens already links (no ast-grep dependency).
//!
//! The slice is deliberately narrow: the pattern is parsed AS CODE with the
//! target grammar, `$UPPERCASE` metavariables become `(_)` wildcards, concrete
//! tokens become `#eq?` text predicates, and the CST structure (node kinds +
//! field names) is emitted verbatim. Repeating a metavariable requires equal
//! text across its occurrences. Raw S-expressions remain the power-user path.
//!
//! Because `$X` is not a valid token in most grammars, metavariables are first
//! substituted with placeholder identifiers (`lens_meta_<i>`, one per
//! occurrence), the substituted snippet is parsed, and the placeholder
//! positions are turned back into wildcards at emission time. Languages where
//! `$X` is itself lexable (php, bash variables) are outside this slice.

use std::fmt::Write as _;

use anyhow::{bail, Context, Result};
use tree_sitter::{Node, Parser, Query};

use super::tags_adapter::AnySpec;

/// Capture name given to the pattern root. The grep engine filters matches to
/// this capture so the bookkeeping captures (`@c0`, `@c1`, ...) never surface.
pub const MATCH_CAPTURE: &str = "match";

/// Placeholder identifier prefix substituted for `$METAVAR` occurrences before
/// parsing. Chosen to lex as a plain identifier in every supported grammar.
const PLACEHOLDER: &str = "lens_meta_";

/// Compile a `$META` pattern into a tree-sitter query string for `spec`'s
/// grammar. The emitted query captures the whole pattern as `@match` and
/// enforces concrete tokens / repeated metavariables via `#eq?` predicates
/// (which the tree-sitter Rust binding applies during matching).
pub fn compile_pattern(pattern: &str, spec: &AnySpec) -> Result<String> {
    let pat = pattern.trim();
    if pat.is_empty() {
        bail!("pattern is empty");
    }
    let (substituted, metavars) = substitute_metavars(pat);

    let lang = spec.language();
    let mut parser = Parser::new();
    parser
        .set_language(&lang)
        .with_context(|| format!("grammar for '{}' failed to load", spec.name()))?;

    // Parse the snippet as-is; grammars whose top level only accepts items
    // (rust) get a second chance inside a function-body wrapper, with the
    // pattern located afterward by its exact byte span.
    let raw = parser
        .parse(&substituted, None)
        .with_context(|| format!("parser failure for '{}'", spec.name()))?;
    let (tree, source, span) = if !raw.root_node().has_error() {
        (raw, substituted.clone(), None)
    } else if let Some((pre, post)) = snippet_wrapper(spec.name()) {
        let wrapped = format!("{pre}{substituted}{post}");
        let tree = parser
            .parse(&wrapped, None)
            .with_context(|| format!("parser failure for '{}'", spec.name()))?;
        if tree.root_node().has_error() {
            bail!("pattern does not parse as {}: `{pat}`", spec.name());
        }
        let start = pre.len();
        (tree, wrapped, Some((start, start + substituted.len())))
    } else {
        bail!("pattern does not parse as {}: `{pat}`", spec.name());
    };

    let root = pattern_root(&tree, span)?;
    let mut emitter = Emitter {
        src: source.as_bytes(),
        metavars: &metavars,
        captures: 0,
        predicates: Vec::new(),
        meta_captures: Vec::new(),
    };
    let body = emitter.emit(root)?;
    let mut predicates = emitter.predicates;
    for (_, caps) in &emitter.meta_captures {
        for later in &caps[1..] {
            predicates.push(format!("(#eq? {} {})", caps[0], later));
        }
    }
    let query = if predicates.is_empty() {
        format!("({body} @{MATCH_CAPTURE})")
    } else {
        format!("({body} @{MATCH_CAPTURE} {})", predicates.join(" "))
    };
    // The emission is grammar-driven, so this only fails on an emitter bug;
    // validating here keeps that failure clear instead of surfacing later as a
    // confusing "invalid query" from the grep engine.
    Query::new(&lang, &query)
        .map_err(|e| anyhow::anyhow!("internal: compiled pattern query invalid ({e}): {query}"))?;
    Ok(query)
}

/// Replace each `$UPPERCASE` metavariable occurrence with a placeholder
/// identifier (`lens_meta_<i>`), returning the substituted text and the
/// metavariable name of each occurrence (index = occurrence order).
fn substitute_metavars(pattern: &str) -> (String, Vec<String>) {
    let mut out = String::with_capacity(pattern.len());
    let mut names: Vec<String> = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().is_some_and(char::is_ascii_uppercase) {
            let mut name = String::new();
            while let Some(&n) = chars.peek() {
                if n.is_ascii_uppercase() || n.is_ascii_digit() || n == '_' {
                    name.push(n);
                    chars.next();
                } else {
                    break;
                }
            }
            let _ = write!(out, "{PLACEHOLDER}{}", names.len());
            names.push(name);
        } else {
            out.push(c);
        }
    }
    (out, names)
}

/// Grammars whose top level rejects bare expressions/statements get the
/// pattern parsed inside this wrapper instead (narrow slice: rust today).
fn snippet_wrapper(lang: &str) -> Option<(&'static str, &'static str)> {
    match lang {
        "rust" => Some(("fn __lens_p__() { ", " }")),
        _ => None,
    }
}

/// Locate the single node the pattern denotes: without a wrapper, the one
/// named child of the file root; with a wrapper, the named node spanning
/// exactly the pattern's bytes. Transparent statement wrappers
/// (`expression_statement`) are unwrapped either way.
fn pattern_root(tree: &tree_sitter::Tree, span: Option<(usize, usize)>) -> Result<Node<'_>> {
    let mut node = match span {
        None => {
            let root = tree.root_node();
            if root.named_child_count() != 1 {
                bail!(
                    "pattern must be a single expression or statement (found {})",
                    root.named_child_count()
                );
            }
            root.named_child(0).expect("count checked")
        }
        Some((start, end)) => {
            let node = tree
                .root_node()
                .named_descendant_for_byte_range(start, end)
                .context("pattern node not found in wrapped parse")?;
            if node.start_byte() != start || node.end_byte() != end {
                bail!("pattern must be a single expression or statement");
            }
            node
        }
    };
    while node.kind() == "expression_statement" && node.named_child_count() == 1 {
        node = node.named_child(0).expect("count checked");
    }
    Ok(node)
}

/// Recursive CST -> query-S-expression emitter. Token leaves become wildcards
/// (metavariables) or text-constrained captures (concrete tokens); interior
/// structure is emitted verbatim with field names; comments (extras) and
/// unfielded anonymous punctuation are skipped.
struct Emitter<'a> {
    src: &'a [u8],
    /// Occurrence index -> metavariable name, from [`substitute_metavars`].
    metavars: &'a [String],
    captures: usize,
    /// Accumulated `#eq?` text predicates for concrete tokens.
    predicates: Vec<String>,
    /// Metavariable name -> capture names, in first-occurrence order.
    meta_captures: Vec<(String, Vec<String>)>,
}

impl Emitter<'_> {
    fn next_capture(&mut self) -> String {
        let cap = format!("@c{}", self.captures);
        self.captures += 1;
        cap
    }

    /// The occurrence index if `text` is exactly a placeholder we generated.
    fn placeholder_occurrence(&self, text: &str) -> Option<usize> {
        let idx: usize = text.strip_prefix(PLACEHOLDER)?.parse().ok()?;
        (idx < self.metavars.len()).then_some(idx)
    }

    fn emit(&mut self, node: Node<'_>) -> Result<String> {
        let text = node.utf8_text(self.src).unwrap_or("");
        if node.child_count() == 0 {
            return self.emit_token(node, text);
        }
        if node.named_child_count() == 0 {
            // Only anonymous children (e.g. rust `arguments` for `()`): match the
            // node kind, not its text — a text `#eq?` would be whitespace-fragile
            // and hand-written queries don't constrain arity here either.
            return Ok(format!("({})", node.kind()));
        }
        let mut out = format!("({}", node.kind());
        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                let child = cursor.node();
                let field = cursor.field_name();
                if child.is_named() {
                    // Extras (comments) in the pattern are not structural.
                    if !child.is_extra() {
                        let part = self.emit(child)?;
                        match field {
                            Some(f) => {
                                let _ = write!(out, " {f}: {part}");
                            }
                            None => {
                                let _ = write!(out, " {part}");
                            }
                        }
                    }
                } else if let Some(f) = field {
                    // A fielded anonymous token carries meaning (e.g.
                    // `operator: "=="`); unfielded punctuation does not.
                    let tok = child.utf8_text(self.src).unwrap_or("");
                    let _ = write!(out, " {f}: \"{}\"", escape(tok));
                }
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
        out.push(')');
        Ok(out)
    }

    /// A leaf token: metavariable wildcard, or concrete text pinned by `#eq?`.
    fn emit_token(&mut self, node: Node<'_>, text: &str) -> Result<String> {
        if let Some(occ) = self.placeholder_occurrence(text) {
            let cap = self.next_capture();
            let name = &self.metavars[occ];
            match self.meta_captures.iter_mut().find(|(m, _)| m == name) {
                Some((_, caps)) => caps.push(cap.clone()),
                None => self.meta_captures.push((name.clone(), vec![cap.clone()])),
            }
            return Ok(format!("(_) {cap}"));
        }
        if text.contains(PLACEHOLDER) {
            bail!(
                "a $METAVAR must stand alone as a whole token, not inside `{}`",
                text.replace(PLACEHOLDER, "$")
            );
        }
        if text.is_empty() {
            return Ok(format!("({})", node.kind()));
        }
        let cap = self.next_capture();
        self.predicates
            .push(format!("(#eq? {cap} \"{}\")", escape(text)));
        Ok(format!("({}) {cap}", node.kind()))
    }
}

/// Escape a token for a double-quoted tree-sitter query string.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::structural::grep_ast_filtered;
    use crate::discovery::tags_adapter::any_spec_for_language;
    use crate::tools::AstMatch;
    use std::fs;
    use tempfile::tempdir;

    fn compile(pattern: &str, lang: &str) -> Result<String> {
        compile_pattern(pattern, &any_spec_for_language(lang).unwrap())
    }

    fn matches(dir: &std::path::Path, pattern: &str, lang: &str) -> Vec<AstMatch> {
        let q = compile(pattern, lang).unwrap();
        grep_ast_filtered(dir, &q, Some(lang), 100, Some(MATCH_CAPTURE)).unwrap()
    }

    /// The T4 oracle row `$X.unwrap()`: the emitted query is deterministic and
    /// carries the wildcard receiver, the pinned method name, and `@match`.
    #[test]
    fn rust_unwrap_emits_expected_query() {
        let q = compile("$X.unwrap()", "rust").unwrap();
        assert_eq!(
            q,
            "((call_expression function: (field_expression value: (_) @c0 \
             field: (field_identifier) @c1) arguments: (arguments)) \
             @match (#eq? @c1 \"unwrap\"))"
        );
    }

    /// Predicate 3: `$X.unwrap()` matches real `.unwrap()` call sites — not
    /// comment mentions, not string literals, not other methods.
    #[test]
    fn rust_unwrap_matches_calls_not_comments() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "// v.unwrap() mentioned in a comment\n\
             fn f(v: Option<i32>, r: Result<i32, i32>) {\n\
                 let _ = v.unwrap();\n\
                 let _ = r.expect(\"other method\");\n\
                 let s = \"w.unwrap() in a string\";\n\
                 let _ = s.len();\n\
             }\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "$X.unwrap()", "rust");
        assert_eq!(hits.len(), 1, "exactly the one real call site: {hits:?}");
        assert_eq!(hits[0].line, 3, "the `.unwrap()` call, not the comment");
    }

    /// The T4 oracle row `print($X)` (python).
    #[test]
    fn python_print_matches_only_print_calls() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.py"),
            "# print(x) in a comment\ndef f(x):\n    print(x)\n    log(x)\n    pprint(x)\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "print($X)", "python");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 3);
    }

    /// The T4 oracle row `$A.map($F)` (typescript).
    #[test]
    fn typescript_map_matches_only_map_calls() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.ts"),
            "// xs.map(f) in a comment\nconst ys = xs.map(f);\nconst zs = xs.filter(f);\n",
        )
        .unwrap();
        let hits = matches(dir.path(), "$A.map($F)", "typescript");
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 2);
    }

    /// A repeated metavariable compiles to an `#eq?` between its captures:
    /// `$X == $X` matches `a == a` but not `a == b` (nor `a != a`).
    #[test]
    fn repeated_metavar_requires_equal_text() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn f(a: i32, b: i32) -> bool {\n\
                 let x = a == a;\n\
                 let y = a == b;\n\
                 let z = a != a;\n\
                 x && y && z\n\
             }\n",
        )
        .unwrap();
        let q = compile("$X == $X", "rust").unwrap();
        assert!(q.contains("(#eq? @c0 @c1)"), "capture-to-capture eq: {q}");
        let hits = grep_ast_filtered(dir.path(), &q, Some("rust"), 100, Some(MATCH_CAPTURE))
            .unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 2, "`a == a` only");
    }

    /// A pattern that fails to parse (ERROR nodes even inside the wrapper)
    /// errors clearly instead of emitting a garbage query.
    #[test]
    fn syntax_error_pattern_is_a_clear_error() {
        let err = compile("fn (", "rust").unwrap_err().to_string();
        assert!(err.contains("does not parse as rust"), "{err}");
    }

    #[test]
    fn empty_pattern_is_a_clear_error() {
        let err = compile("   ", "rust").unwrap_err().to_string();
        assert!(err.contains("pattern is empty"), "{err}");
    }

    #[test]
    fn multiple_roots_is_a_clear_error() {
        let err = compile("a()\nb()", "python").unwrap_err().to_string();
        assert!(err.contains("single expression or statement"), "{err}");
    }

    /// A metavariable glued to other identifier characters cannot survive the
    /// lexer as its own token; that must be an error, not a silent mismatch.
    #[test]
    fn embedded_metavar_is_a_clear_error() {
        let err = compile("foo_$X()", "python").unwrap_err().to_string();
        assert!(err.contains("whole token"), "{err}");
    }
}
