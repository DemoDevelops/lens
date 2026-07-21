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
    grep_ast_filtered(root, query, language, limit, None)
}

/// As [`grep_ast`], but when `only_capture` is given only captures with that name
/// produce matches. The compiled `$META`-pattern path uses this: its queries
/// capture the whole pattern as `@match` plus bookkeeping captures (`@c0`, ...)
/// that feed `#eq?` predicates and must not surface as results.
pub fn grep_ast_filtered(
    root: &Path,
    query: &str,
    language: Option<&str>,
    limit: usize,
    only_capture: Option<&str>,
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
        let tree = match parser.parse(&source, None) {
            Some(t) => t,
            None => continue,
        };
        let rel = file
            .strip_prefix(&base)
            .unwrap_or(&file)
            .to_string_lossy()
            .to_string();
        let src = source.as_bytes();
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(&q, tree.root_node(), src);
        while let Some(m) = it.next() {
            for cap in m.captures {
                if let Some(want) = only_capture {
                    if q.capture_names()[cap.index as usize] != want {
                        continue;
                    }
                }
                let node = cap.node;
                let line = node.start_position().row + 1;
                let text: String = node.utf8_text(src).unwrap_or("").chars().take(120).collect();
                if !seen.insert((rel.clone(), line, text.clone())) {
                    continue;
                }
                out.push(AstMatch {
                    path: rel.clone(),
                    line,
                    text,
                });
                if out.len() >= limit {
                    return Ok(out);
                }
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
    Ok(out)
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
