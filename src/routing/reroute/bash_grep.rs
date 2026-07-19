//! bash_grep: shell-command grep segment → lens_search/lens_symbol/lens_grep_ast
//! classifier. Rail 1c of the reroute family (see [`super`] for the
//! counter-key/env-flag contract).
//!
//! `bash_decision` (`src/routing/mod.rs`) sees a Bash `command` as raw text,
//! not the structured `pattern`/`path` the `Grep` tool gets — so before it can
//! reuse the existing grep-shape rails (`grep_symbol`, `grep_ast`) it needs a
//! segment parsed into the same pattern/path shape those rails already
//! classify. [`parse_grep_seg`] does that parse (no I/O, no `RouteCtx`);
//! [`classify`] then answers "which lens call replaces this segment", reusing
//! [`super::grep_symbol::symbol_grep`] and [`super::grep_ast::syntax_shape`]
//! rather than reinventing shape detection.
//!
//! Symbol promotion is deliberately narrower here than the `Grep`-tool rail:
//! only a definition shape (`fn foo`, `class Bar`, …) promotes to
//! [`BashGrepClass::Symbol`]. A bare identifier alone (`grep_symbol`'s
//! [`super::grep_symbol::SymbolKind::Bare`]) does not — a shell `grep -rn foo
//! src/` is an extremely common, low-signal shape (any single word), unlike
//! the `Grep` tool's `pattern` field where the model typed exactly that
//! identifier as its whole search target. Bare-shaped Bash greps fall through
//! to [`BashGrepClass::Broad`] instead.

use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use super::grep_ast;
use super::grep_symbol;

/// A Bash segment recognized as a grep-family invocation: the extracted
/// pattern/path arguments plus whether its scope is broad (repo/directory) or
/// narrow (a single concrete file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GrepSeg {
    pub(crate) pattern: String,
    pub(crate) path: Option<String>,
    pub(crate) broad: bool,
}

/// Shape of a Bash segment's classified target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BashGrepClass {
    /// Broad-scope text search — target `lens_search`.
    Broad,
    /// Definition-shaped pattern — target `lens_symbol(name=ident)`.
    Symbol { ident: String },
    /// Syntax-shaped pattern — target `lens_grep_ast`.
    Ast,
    /// Single-file scope — the deliberate escape hatch, stays passthrough.
    NarrowExact,
}

/// Parse a Bash segment into a [`GrepSeg`] when it invokes `grep`/`rg`/
/// `egrep`/`fgrep` or the two-word `git grep`. Skips option flags (anything
/// starting with `-`, attached-value flags like `--include='*.rs'` included)
/// to find the PATTERN and optional PATH positional arguments. Returns `None`
/// for anything else — a different command, or a grep invocation with no
/// pattern.
pub(crate) fn parse_grep_seg(seg: &str) -> Option<GrepSeg> {
    let tokens = shell_words::split(seg.trim()).ok()?;
    let mut iter = tokens.iter();
    let first = iter.next()?.as_str();
    let is_family = match first {
        "grep" | "rg" | "egrep" | "fgrep" => true,
        "git" if iter.clone().next().map(String::as_str) == Some("grep") => {
            iter.next();
            true
        }
        _ => false,
    };
    if !is_family {
        return None;
    }

    let positionals: Vec<&str> = iter
        .map(String::as_str)
        .filter(|tok| !tok.starts_with('-'))
        .collect();
    let pattern = (*positionals.first()?).to_string();
    let path = positionals.get(1).map(|s| (*s).to_string());
    let broad = is_broad_path(path.as_deref());
    Some(GrepSeg { pattern, path, broad })
}

/// Missing, `.`, or directory-looking (trailing `/`, no extension) → broad. A
/// single concrete file (has an extension, no trailing `/`) → narrow.
fn is_broad_path(path: Option<&str>) -> bool {
    match path {
        None => true,
        Some(".") => true,
        Some(p) => p.ends_with('/') || Path::new(p).extension().is_none(),
    }
}

/// The identifier named by a definition-shaped pattern (`fn foo` → `foo`),
/// mirroring `grep_symbol`'s private `def_re` capture — duplicated locally
/// since that helper isn't exported, only called once `symbol_grep` has
/// already confirmed the shape.
fn def_identifier(pattern: &str) -> Option<String> {
    static DEF_RE: OnceLock<Regex> = OnceLock::new();
    let re = DEF_RE.get_or_init(|| {
        Regex::new(
            r"\b(?:fn|func|def|class|struct|impl|trait|type|enum|const|interface|function)\b\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("static def regex compiles")
    });
    re.captures(pattern.trim())
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Classify a parsed [`GrepSeg`]: single-file scope always wins as
/// [`BashGrepClass::NarrowExact`] (the escape hatch), even when the pattern
/// also looks symbol/ast shaped. Otherwise a definition-shaped pattern is
/// [`BashGrepClass::Symbol`], a syntax-shaped pattern is
/// [`BashGrepClass::Ast`], and anything else broad-scope is
/// [`BashGrepClass::Broad`].
pub(crate) fn classify(seg: &GrepSeg) -> BashGrepClass {
    if !seg.broad {
        return BashGrepClass::NarrowExact;
    }
    if matches!(
        grep_symbol::symbol_grep(&seg.pattern),
        Some(grep_symbol::SymbolKind::Def)
    ) {
        if let Some(ident) = def_identifier(&seg.pattern) {
            return BashGrepClass::Symbol { ident };
        }
    }
    if grep_ast::syntax_shape(&seg.pattern).is_some() {
        return BashGrepClass::Ast;
    }
    BashGrepClass::Broad
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broad_grep_with_directory_path() {
        let seg = parse_grep_seg("grep -rn foo src/").unwrap();
        assert_eq!(seg.pattern, "foo");
        assert_eq!(seg.path.as_deref(), Some("src/"));
        assert!(seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::Broad);
    }

    #[test]
    fn broad_rg_with_no_path() {
        let seg = parse_grep_seg("rg -n foo").unwrap();
        assert_eq!(seg.pattern, "foo");
        assert_eq!(seg.path, None);
        assert!(seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::Broad);
    }

    #[test]
    fn broad_git_grep() {
        let seg = parse_grep_seg("git grep foo").unwrap();
        assert_eq!(seg.pattern, "foo");
        assert_eq!(seg.path, None);
        assert!(seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::Broad);
    }

    #[test]
    fn symbol_shaped_definition_pattern() {
        let seg = parse_grep_seg("grep -rn \"fn parse_grep_seg\"").unwrap();
        assert_eq!(seg.pattern, "fn parse_grep_seg");
        assert!(seg.broad);
        assert_eq!(
            classify(&seg),
            BashGrepClass::Symbol {
                ident: "parse_grep_seg".to_string()
            }
        );
    }

    #[test]
    fn ast_shaped_pattern() {
        let seg = parse_grep_seg("grep -rn \".unwrap()\" .").unwrap();
        assert_eq!(seg.pattern, ".unwrap()");
        assert!(seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::Ast);
    }

    #[test]
    fn narrow_single_file_scope() {
        let seg = parse_grep_seg("grep foo src/main.rs").unwrap();
        assert_eq!(seg.pattern, "foo");
        assert_eq!(seg.path.as_deref(), Some("src/main.rs"));
        assert!(!seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::NarrowExact);
    }

    #[test]
    fn flag_soup_skips_attached_value_flags() {
        let seg = parse_grep_seg("grep -rn --include='*.rs' foo .").unwrap();
        assert_eq!(seg.pattern, "foo");
        assert_eq!(seg.path.as_deref(), Some("."));
        assert!(seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::Broad);
    }

    #[test]
    fn non_grep_command_is_none() {
        assert!(parse_grep_seg("ls -la src/").is_none());
    }

    #[test]
    fn narrow_scope_wins_over_symbol_shape() {
        // Single-file scope stays NarrowExact even when the pattern also
        // looks definition-shaped.
        let seg = parse_grep_seg("grep \"fn parse_grep_seg\" src/routing/reroute/bash_grep.rs")
            .unwrap();
        assert!(!seg.broad);
        assert_eq!(classify(&seg), BashGrepClass::NarrowExact);
    }
}
