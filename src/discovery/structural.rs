//! `lens_grep_ast`: structural (tree-sitter) search. Runs an AST query over the
//! repo and returns `path:line` matches, which a textual grep cannot do: it
//! matches syntax (a `.unwrap()` call, a function returning `Result`), not text,
//! so comments and strings that merely mention a token never match.

use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use super::tags_adapter::{any_spec_for_extension, any_spec_for_language};
use crate::tools::AstMatch;

/// Run tree-sitter `query` over the supported source files under `root`, returning
/// one [`AstMatch`] per distinct capture site (path, 1-based line, capped node
/// text), up to `limit`. Duplicate captures of the same site are dropped BEFORE
/// they count toward `limit`: unanchored sibling matching (e.g. adjacent `$$$`
/// variadic groups, which multiply raw matches 2^(k-1)-fold) must not evict
/// genuine later matches. When `language` is given only that language's files are
/// searched and the query is validated up front; otherwise each file is matched
/// against the query compiled for its own grammar, and files whose grammar can't
/// compile the query are skipped. Deterministic: files are walked in sorted order.
pub fn grep_ast(
    root: &Path,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<Vec<AstMatch>> {
    grep_ast_filtered(root, query, language, limit, None, false)
}

/// As [`grep_ast`], but when `only_capture` is given only captures with that name
/// produce matches. The compiled `$META`-pattern path uses this: its queries
/// capture the whole pattern as `@match` plus bookkeeping captures (`@c0`, ...)
/// that feed `#eq?` predicates and must not surface as results.
///
/// Every match carries an origin ("prod"/"test"/"bench"): bench for files with a
/// `benchmarks`/`tests`/`fixtures` segment in the reported relative path (the
/// graph's [`super::is_bench_path`] rule), test for Rust captures inside a
/// `#[cfg(test)]` span (the graph's `Origin::Test` machinery). Labels are
/// all-or-none: stripped when every match is prod. `prod_only` drops non-prod
/// matches BEFORE they count toward `limit`.
pub fn grep_ast_filtered(
    root: &Path,
    query: &str,
    language: Option<&str>,
    limit: usize,
    only_capture: Option<&str>,
    prod_only: bool,
) -> Result<Vec<AstMatch>> {
    if !root.exists() {
        anyhow::bail!("grep_ast root does not exist: {}", root.display());
    }
    // If a language is named, validate the query up front so a malformed query
    // errors clearly instead of silently matching nothing.
    if let Some(lang) = language {
        let spec = any_spec_for_language(lang)
            .with_context(|| format!("unsupported language '{lang}'"))?;
        Query::new(&spec.language(), query).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("Impossible pattern") {
                anyhow::anyhow!(
                    "invalid query for {lang}: {e}. lens hint: predicates like `#eq?` \
                     must sit at the pattern's top level, not nested inside a capture. \
                     Hoist them — wrong: `((identifier) @a (#eq? @a \"foo\"))`; \
                     right: `((identifier) @a) (#eq? @a \"foo\")`."
                )
            } else {
                anyhow::anyhow!("invalid query for {lang}: {e}")
            }
        })?;
    }
    let want = language.map(|l| l.to_ascii_lowercase());

    // The base for relative paths: the dir itself, or a file's parent.
    let base: PathBuf = if root.is_file() {
        root.parent().unwrap_or(root).to_path_buf()
    } else {
        root.to_path_buf()
    };

    let mut files: Vec<PathBuf> = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
    } else {
        let mut builder = WalkBuilder::new(root);
        builder.standard_filters(true);
        for entry in builder.build().flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                files.push(entry.into_path());
            }
        }
    }
    files.sort();

    let mut out: Vec<AstMatch> = Vec::new();
    // Dedup key per capture site. AstMatch carries no byte range, so
    // (path, line, text) stands in: a true duplicate capture (the same node
    // matched again) is identical on all three.
    let mut seen: std::collections::HashSet<(String, usize, String)> =
        std::collections::HashSet::new();
    // When no language is named, track per-grammar compile failures so a query
    // that fails under every encountered grammar errors instead of silently
    // returning `[]`.
    let mut any_compile_ok = false;
    let mut saw_grammar = false;
    let mut compile_errs: BTreeSet<String> = BTreeSet::new();
    for file in files {
        let ext = match file.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let spec = match any_spec_for_extension(ext) {
            Some(s) => s,
            None => continue,
        };
        if let Some(w) = &want {
            if spec.name() != w {
                continue;
            }
        }
        let source = match std::fs::read(&file) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => continue,
            },
            Err(_) => continue,
        };
        let lang = spec.language();
        saw_grammar = true;
        // Skip files whose grammar can't compile this query (only happens when no
        // language was named and the query is grammar-specific).
        let q = match Query::new(&lang, query) {
            Ok(q) => {
                any_compile_ok = true;
                q
            }
            Err(e) => {
                compile_errs.insert(format!("{}: {e}", spec.name()));
                continue;
            }
        };
        let mut parser = Parser::new();
        if parser.set_language(&lang).is_err() {
            continue;
        }
        let rel = file
            .strip_prefix(&base)
            .unwrap_or(&file)
            .to_string_lossy()
            .to_string();
        // Provenance for this file's matches: bench by path segment on the
        // reported relative path, test by `#[cfg(test)]` byte span (Rust only,
        // off the depth-0 tree, in absolute file bytes).
        let bench_file = super::is_bench_path(&rel);
        let mut cfg_test_spans: Vec<(usize, usize)> = Vec::new();
        // Virtual documents: the file itself, then (Rust) each macro-invocation
        // token-tree interior re-parsed as source. Macro bodies parse as opaque
        // token trees, so the file's own tree cannot match a call like
        // `matches!(std::env::var("X"), …)`; tree-sitter's error recovery still
        // shapes the well-formed code inside a re-parsed interior, and the
        // line/byte offsets map matches back to real file positions (so
        // cfg(test) spans keep working on absolute bytes). Depth-bounded:
        // nested macros re-parse again via their own interiors.
        let mut docs: std::collections::VecDeque<VirtualDoc> = std::collections::VecDeque::new();
        docs.push_back(VirtualDoc { text: source, line_off: 0, byte_off: 0, depth: 0 });
        while let Some(doc) = docs.pop_front() {
            let tree = match parser.parse(&doc.text, None) {
                Some(t) => t,
                None => continue,
            };
            let src = doc.text.as_bytes();
            if doc.depth == 0 && spec.name() == "rust" {
                cfg_test_spans = super::extract::collect_cfg_test_spans(&tree.root_node(), src);
            }
            let mut cursor = QueryCursor::new();
            let mut it = cursor.matches(&q, tree.root_node(), src);
            while let Some(m) = it.next() {
                // One row per query MATCH, not per capture: a raw multi-capture
                // query (`(function_item (visibility_modifier) @vis name:
                // (identifier) @name) @fn`) otherwise emits three rows per site
                // and `count` triples (2026-07-21 audit: 117 reported vs 39 real
                // pub fns). With `only_capture` (the pattern-DSL path) keep the
                // named capture; otherwise represent the match by its OUTERMOST
                // captured node (earliest start, longest span).
                let picked: Vec<_> = if let Some(want) = only_capture {
                    m.captures
                        .iter()
                        .filter(|c| q.capture_names()[c.index as usize] == want)
                        .collect()
                } else {
                    m.captures
                        .iter()
                        .min_by_key(|c| (c.node.start_byte(), std::cmp::Reverse(c.node.end_byte())))
                        .into_iter()
                        .collect()
                };
                // Raw multi-capture queries asked for several things by name;
                // surface every capture's text on the row so the answer does
                // not collapse to the representative node alone (2026-07-21
                // audit: a `@name @r` query returned names without return
                // types, forcing a full-skeleton follow-up).
                let captures = if only_capture.is_none() && m.captures.len() > 1 {
                    let mut map = std::collections::BTreeMap::new();
                    for c in m.captures {
                        let name = q.capture_names()[c.index as usize].to_string();
                        let t: String =
                            c.node.utf8_text(src).unwrap_or("").chars().take(80).collect();
                        map.entry(name).or_insert(t);
                    }
                    Some(map)
                } else {
                    None
                };
                for cap in picked {
                    let node = cap.node;
                    // Bench (whole-file provenance) wins over an inner cfg(test)
                    // span, matching the graph's node-origin precedence.
                    let origin = if bench_file {
                        "bench"
                    } else if super::extract::byte_in_spans(
                        doc.byte_off + node.start_byte(),
                        &cfg_test_spans,
                    ) {
                        "test"
                    } else {
                        "prod"
                    };
                    if prod_only && origin != "prod" {
                        continue;
                    }
                    let line = doc.line_off + node.start_position().row + 1;
                    let text: String =
                        node.utf8_text(src).unwrap_or("").chars().take(120).collect();
                    if !seen.insert((rel.clone(), line, text.clone())) {
                        continue;
                    }
                    out.push(AstMatch {
                        path: rel.clone(),
                        line,
                        text,
                        origin: Some(origin.to_string()),
                        captures: captures.clone(),
                    });
                    if out.len() >= limit {
                        return Ok(strip_all_prod_origins(out));
                    }
                }
            }
            if spec.name() == "rust" && doc.depth < 2 {
                push_macro_interiors(tree.root_node(), &doc, &mut docs);
            }
        }
    }
    if language.is_none() && saw_grammar && !any_compile_ok && !compile_errs.is_empty() {
        let joined = compile_errs.into_iter().collect::<Vec<_>>().join("; ");
        // Same hint as the up-front single-language validation above; the
        // aggregate path must not degrade to the raw tree-sitter error.
        if joined.contains("Impossible pattern") {
            bail!(
                "query failed to compile for every encountered grammar: {joined}. \
                 lens hint: predicates like `#eq?` must sit at the pattern's top level, \
                 not nested inside a capture. Hoist them — wrong: \
                 `((identifier) @a (#eq? @a \"foo\"))`; \
                 right: `((identifier) @a) (#eq? @a \"foo\")`."
            );
        }
        bail!("query failed to compile for every encountered grammar: {joined}");
    }
    Ok(strip_all_prod_origins(out))
}

/// One source text to run the query over: the file itself (offsets 0), or a
/// macro-invocation token-tree interior with offsets mapping its positions back
/// to the enclosing file.
struct VirtualDoc {
    text: String,
    /// Rows in the file before this doc's first row.
    line_off: usize,
    /// File byte offset of this doc's first byte (for cfg(test) span checks).
    byte_off: usize,
    /// Re-parse nesting level (0 = the file itself).
    depth: u8,
}

/// Queue each `macro_invocation` token-tree interior in `node`'s subtree as a
/// [`VirtualDoc`] to re-parse. The interior excludes the one-byte delimiters
/// (`(…)`, `[…]`, `{…}`), so it starts on the token tree's own line. Nested
/// macro invocations don't appear inside a token tree's CST (its contents are
/// tokens); they surface in the interior's re-parse, one depth level down.
fn push_macro_interiors(
    node: tree_sitter::Node<'_>,
    doc: &VirtualDoc,
    docs: &mut std::collections::VecDeque<VirtualDoc>,
) {
    if node.kind() == "macro_invocation" {
        for i in 0..node.child_count() {
            let ch = node.child(i).expect("count checked");
            if ch.kind() != "token_tree" {
                continue;
            }
            let (s, e) = (ch.start_byte() + 1, ch.end_byte().saturating_sub(1));
            if s >= e {
                continue;
            }
            let interior = &doc.text[s..e];
            if interior.trim().is_empty() {
                continue;
            }
            docs.push_back(VirtualDoc {
                text: interior.to_string(),
                line_off: doc.line_off + ch.start_position().row,
                byte_off: doc.byte_off + s,
                depth: doc.depth + 1,
            });
        }
        return;
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            push_macro_interiors(c.node(), doc, docs);
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

/// All-or-none origin labeling, like the graph views: when every match is prod,
/// drop the labels so "no `origin` fields" keeps meaning "all production code".
fn strip_all_prod_origins(mut matches: Vec<AstMatch>) -> Vec<AstMatch> {
    if matches.iter().all(|m| m.origin.as_deref() == Some("prod")) {
        for m in &mut matches {
            m.origin = None;
        }
    }
    matches
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn finds_method_calls_not_comment_mentions() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "// unwrap mentioned in a comment\nfn f() {\n    let v: Option<i32> = Some(1);\n    v.unwrap()\n}\n",
        )
        .unwrap();
        let q = "(call_expression function: (field_expression field: (field_identifier) @m))";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        let unwraps: Vec<&AstMatch> = hits.iter().filter(|m| m.text == "unwrap").collect();
        assert_eq!(unwraps.len(), 1, "exactly one .unwrap() call, got {hits:?}");
        assert_eq!(unwraps[0].line, 4, "the call is on line 4, not the comment");
    }

    #[test]
    fn invalid_query_errors_with_language() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn f() {}\n").unwrap();
        let res = grep_ast(dir.path(), "(not_a_real_node) @x", Some("rust"), 100);
        assert!(res.is_err(), "an invalid query must error when a language is named");
    }

    /// Nested `#eq?` (mined GA-SEXPR shape) triggers tree-sitter's
    /// "Impossible pattern"; the error must tell the user to hoist the
    /// predicate to the top level.
    #[test]
    fn nested_eq_predicate_is_a_clear_error() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn f() {}\n").unwrap();
        // Nested predicate inside a fielded capture — the shape models write.
        let q = r#"(function_item return_type: (generic_type type: (type_identifier) @r (#eq? @r "Result")) name: (identifier) @name)"#;
        let err = grep_ast(dir.path(), q, Some("rust"), 100)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Impossible pattern") || err.to_lowercase().contains("impossible"),
            "tree-sitter should reject nested predicates: {err}"
        );
        assert!(
            err.contains("hoist") || err.contains("top level"),
            "must mention hoisting predicates to top level: {err}"
        );
    }

    /// With no language named, a query that fails under every encountered
    /// grammar must error (not silently return `[]`).
    #[test]
    fn no_language_bogus_query_is_a_clear_error() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn f() {}\n").unwrap();
        let err = grep_ast(dir.path(), "(not_a_real_node_zzzz) @x", None, 100)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("every encountered grammar") || err.contains("failed to compile"),
            "must aggregate compile failures, not return empty: {err}"
        );
    }

    /// A raw query with several captures per pattern reports ONE match per
    /// site (the outermost captured node), so `count` equals real sites: the
    /// 2026-07-21 audit shape `@vis @name @fn` reported 3x the true pub-fn
    /// count.
    #[test]
    fn multi_capture_query_counts_sites_not_captures() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "pub fn one() {}\npub fn two() {}\nfn private() {}\n",
        )
        .unwrap();
        let q = "(function_item (visibility_modifier) @vis name: (identifier) @name) @fn";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        assert_eq!(hits.len(), 2, "one row per pub fn, not per capture: {hits:?}");
        assert!(
            hits[0].text.starts_with("pub fn one"),
            "the outermost node represents the match: {hits:?}"
        );
        // Every named capture's text rides on the row, so a multi-capture
        // query answers all its questions in one call (the 0072 audit run got
        // names without return types and burned a full-skeleton round).
        let caps = hits[0].captures.as_ref().expect("multi-capture rows carry captures");
        assert_eq!(caps.get("name").map(String::as_str), Some("one"), "{hits:?}");
        assert_eq!(caps.get("vis").map(String::as_str), Some("pub"), "{hits:?}");
    }

    /// Rust macro invocation bodies parse as opaque token trees; the interior
    /// re-parse must still find a call written inside one, at its real file
    /// line, with cfg(test)-span origins intact.
    #[test]
    fn matches_inside_macro_invocation_bodies() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn explain_on() -> bool {\n    matches!(std::env::var(\"LENS_EXPLAIN\"), Ok(v) if v == \"1\")\n}\n#[cfg(test)]\nmod tests {\n    fn t() -> bool {\n        matches!(std::env::var(\"LENS_TEST_ONLY\"), Ok(_))\n    }\n}\n",
        )
        .unwrap();
        let q = "(call_expression function: (scoped_identifier)) @c";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        let explain: Vec<&AstMatch> =
            hits.iter().filter(|m| m.text.contains("LENS_EXPLAIN")).collect();
        assert_eq!(explain.len(), 1, "the matches!-wrapped call must be found: {hits:?}");
        assert_eq!(explain[0].line, 2, "line maps back to the real file: {hits:?}");
        assert_eq!(explain[0].origin.as_deref(), Some("prod"), "{hits:?}");
        let test_only: Vec<&AstMatch> =
            hits.iter().filter(|m| m.text.contains("LENS_TEST_ONLY")).collect();
        assert_eq!(test_only[0].origin.as_deref(), Some("test"), "{hits:?}");
        let prod = grep_ast_filtered(dir.path(), q, Some("rust"), 100, None, true).unwrap();
        assert!(
            prod.iter().any(|m| m.text.contains("LENS_EXPLAIN"))
                && !prod.iter().any(|m| m.text.contains("LENS_TEST_ONLY")),
            "prod_only keeps working on macro-interior matches: {prod:?}"
        );

        // The compiled $META pattern path reaches macro interiors too.
        let pat_hits = {
            let spec = crate::discovery::tags_adapter::any_spec_for_language("rust").unwrap();
            let compiled = crate::discovery::pattern::compile_pattern("std::env::var($X)", &spec).unwrap();
            grep_ast_filtered(
                dir.path(),
                &compiled,
                Some("rust"),
                100,
                Some(crate::discovery::pattern::MATCH_CAPTURE),
                true,
            )
            .unwrap()
        };
        assert!(
            pat_hits.iter().any(|m| m.text.contains("LENS_EXPLAIN")),
            "pattern-DSL matches inside macros: {pat_hits:?}"
        );
    }

    /// A macro nested inside another macro's body is one re-parse level down;
    /// the bounded recursion still reaches it.
    #[test]
    fn matches_inside_nested_macro_bodies() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn f(x: Option<i32>) {\n    assert!(matches!(x.unwrap(), 1));\n}\n",
        )
        .unwrap();
        let q = "(call_expression function: (field_expression field: (field_identifier) @m))";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        assert!(
            hits.iter().any(|m| m.text == "unwrap" && m.line == 2),
            "the assert!(matches!(…)) interior call must be found: {hits:?}"
        );
    }

    /// Rust matches inside a `#[cfg(test)]` span are test-origin; when any
    /// non-prod match is present every match carries a label, and when all
    /// matches are prod none does (all-or-none, like the graph views).
    #[test]
    fn origin_labels_cfg_test_spans_all_or_none() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn prod() { x.unwrap() }\n#[cfg(test)]\nmod tests {\n    fn t() { y.unwrap() }\n}\n",
        )
        .unwrap();
        let q = "(call_expression function: (field_expression field: (field_identifier) @m))";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        assert_eq!(hits[0].origin.as_deref(), Some("prod"), "{hits:?}");
        assert_eq!(hits[1].origin.as_deref(), Some("test"), "{hits:?}");

        let all_prod = tempdir().unwrap();
        fs::write(all_prod.path().join("a.rs"), "fn prod() { x.unwrap() }\n").unwrap();
        let hits = grep_ast(all_prod.path(), q, Some("rust"), 100).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].origin.is_none(), "all-prod results carry no labels: {hits:?}");
    }

    /// `prod_only` drops test matches BEFORE they count toward `limit`: a limit
    /// of 1 must still surface the prod match sitting after a test-span one.
    #[test]
    fn prod_only_filters_before_limit() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "#[cfg(test)]\nmod tests {\n    fn t() { y.unwrap() }\n}\nfn prod() { x.unwrap() }\n",
        )
        .unwrap();
        let q = "(call_expression function: (field_expression field: (field_identifier) @m))";
        let hits = grep_ast_filtered(dir.path(), q, Some("rust"), 1, None, true).unwrap();
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].line, 5, "the prod call, not the test-span one: {hits:?}");
        assert!(hits[0].origin.is_none(), "prod-only results are all-prod: {hits:?}");
    }

    /// Files under a `tests/` (or `benchmarks/`/`fixtures/`) directory are
    /// bench-origin by path, and `prod_only` excludes them.
    #[test]
    fn bench_path_origin_and_prod_only() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("tests")).unwrap();
        fs::write(dir.path().join("prod.rs"), "fn p() { x.unwrap() }\n").unwrap();
        fs::write(dir.path().join("tests").join("i.rs"), "fn t() { y.unwrap() }\n").unwrap();
        let q = "(call_expression function: (field_expression field: (field_identifier) @m))";
        let hits = grep_ast(dir.path(), q, Some("rust"), 100).unwrap();
        assert_eq!(hits.len(), 2, "{hits:?}");
        let bench: Vec<&AstMatch> =
            hits.iter().filter(|m| m.origin.as_deref() == Some("bench")).collect();
        assert_eq!(bench.len(), 1, "{hits:?}");
        assert!(bench[0].path.contains("tests"), "{hits:?}");
        let prod = grep_ast_filtered(dir.path(), q, Some("rust"), 100, None, true).unwrap();
        assert_eq!(prod.len(), 1, "{prod:?}");
        assert_eq!(prod[0].path, "prod.rs", "{prod:?}");
    }

    #[test]
    fn grep_ast_supports_tags_languages() {
        // grep_ast resolves the grammar for tags-adapter languages too (not just the
        // 6 hand-written), so a C query works.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.c"), "int add(int a, int b) { return a + b; }\n").unwrap();
        let hits = grep_ast(
            dir.path(),
            "(function_declarator declarator: (identifier) @name)",
            Some("c"),
            100,
        )
        .unwrap();
        assert!(hits.iter().any(|m| m.text == "add"), "expected C function `add`, got {hits:?}");
    }
}
