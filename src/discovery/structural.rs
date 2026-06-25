//! `lens_grep_ast`: structural (tree-sitter) search. Runs an AST query over the
//! repo and returns `path:line` matches, which a textual grep cannot do: it
//! matches syntax (a `.unwrap()` call, a function returning `Result`), not text,
//! so comments and strings that merely mention a token never match.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::WalkBuilder;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Parser, Query, QueryCursor};

use super::extract::{spec_for_extension, spec_for_language};
use crate::tools::AstMatch;

/// Run tree-sitter `query` over the supported source files under `root`, returning
/// one [`AstMatch`] per capture (path, 1-based line, capped node text), up to
/// `limit`. When `language` is given only that language's files are searched and
/// the query is validated up front; otherwise each file is matched against the
/// query compiled for its own grammar, and files whose grammar can't compile the
/// query are skipped. Deterministic: files are walked in sorted order.
pub fn grep_ast(
    root: &Path,
    query: &str,
    language: Option<&str>,
    limit: usize,
) -> Result<Vec<AstMatch>> {
    if !root.exists() {
        anyhow::bail!("grep_ast root does not exist: {}", root.display());
    }
    // If a language is named, validate the query up front so a malformed query
    // errors clearly instead of silently matching nothing.
    if let Some(lang) = language {
        let spec =
            spec_for_language(lang).with_context(|| format!("unsupported language '{lang}'"))?;
        Query::new(&(spec.language)(), query)
            .map_err(|e| anyhow::anyhow!("invalid query for {lang}: {e}"))?;
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
    for file in files {
        let ext = match file.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let spec = match spec_for_extension(ext) {
            Some(s) => s,
            None => continue,
        };
        if let Some(w) = &want {
            if spec.name != w {
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
        let lang = (spec.language)();
        // Skip files whose grammar can't compile this query (only happens when no
        // language was named and the query is grammar-specific).
        let q = match Query::new(&lang, query) {
            Ok(q) => q,
            Err(_) => continue,
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
                let node = cap.node;
                let line = node.start_position().row + 1;
                let text: String = node.utf8_text(src).unwrap_or("").chars().take(120).collect();
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
}
