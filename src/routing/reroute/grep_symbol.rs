//! grep_symbol: Grep(symbol) → lens_symbol/lens_find classifier.
//!
//! Rail 1a of the reroute family (see [`super`] for the counter-key/env-flag
//! contract). [`symbol_grep`] recognizes a Grep `pattern` that is really a
//! symbol lookup — a definition shape (`fn foo`, `class Bar`, …) or a bare
//! identifier — the case the graph answers directly instead of a grep→Read
//! chain. [`reason`] renders the one-shot deny text T8 attaches to the
//! `Decision::Deny`, naming the exact lens call (with args) rather than a
//! generic tool list — mirrors the shape of `GREP_FIRST_DENY_REASON`
//! (`src/routing/mod.rs`: state the trigger, name the call, offer the
//! `ToolSearch` bootstrap, note it's one-shot) but keyed on the call's pattern
//! instead of the prompt's phrasing.

use std::sync::OnceLock;

use regex::Regex;

/// Shape of a Grep `pattern` recognized as a symbol lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    /// A definition shape: a `fn`/`class`/`struct`/… keyword followed by a name.
    Def,
    /// The whole (trimmed) pattern is a single bare identifier.
    Bare,
}

/// Definition keywords also excluded from [`SymbolKind::Bare`] — a lone
/// keyword (e.g. a grep for just `"fn"`) is not itself a symbol lookup.
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "func",
    "def",
    "class",
    "struct",
    "impl",
    "trait",
    "type",
    "enum",
    "const",
    "interface",
    "function",
];

/// Common English words that also parse as identifiers — excluded from
/// [`SymbolKind::Bare`] so a stray short word in a prose-shaped grep doesn't
/// route as a symbol lookup.
const STOPWORDS: &[&str] = &["the", "and", "for", "not", "let", "use", "mut", "pub"];

/// A definition: keyword, then the identifier it names (captured for [`reason`]).
static DEF_RE: OnceLock<Regex> = OnceLock::new();

fn def_re() -> &'static Regex {
    DEF_RE.get_or_init(|| {
        Regex::new(
            r"\b(?:fn|func|def|class|struct|impl|trait|type|enum|const|interface|function)\b\s+([A-Za-z_][A-Za-z0-9_]*)",
        )
        .expect("static def regex compiles")
    })
}

/// A single bare identifier spanning the whole (trimmed) pattern.
static BARE_RE: OnceLock<Regex> = OnceLock::new();

fn bare_re() -> &'static Regex {
    BARE_RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static bare regex compiles")
    })
}

/// Classify a Grep `pattern`: a definition shape → [`SymbolKind::Def`]; else a
/// single identifier (len ≥ 3, not a lone keyword or stopword) →
/// [`SymbolKind::Bare`]; else `None` (a regex/text search, not a symbol
/// lookup). Def is checked first so a definition line is never miscounted as
/// a bare identifier.
pub fn symbol_grep(pattern: &str) -> Option<SymbolKind> {
    let trimmed = pattern.trim();
    if def_re().is_match(trimmed) {
        return Some(SymbolKind::Def);
    }
    if bare_re().is_match(trimmed)
        && trimmed.len() >= 3
        && !DEF_KEYWORDS.contains(&trimmed)
        && !STOPWORDS.contains(&trimmed)
    {
        return Some(SymbolKind::Bare);
    }
    None
}

/// Best-effort identifier to embed as `lens_symbol(name="...")`, and whether
/// it is a clean identifier (so `lens_symbol` alone suffices) or a fallback
/// (so [`reason`] also offers `lens_find`, which takes a free-text query
/// instead of a name).
fn extract_identifier(trimmed: &str) -> (String, bool) {
    if let Some(name) = def_re().captures(trimmed).and_then(|c| c.get(1)) {
        return (name.as_str().to_string(), true);
    }
    if bare_re().is_match(trimmed) {
        return (trimmed.to_string(), true);
    }
    (trimmed.to_string(), false)
}

/// Deny reason for a Grep `pattern` [`symbol_grep`] identified as a symbol
/// lookup: names the exact `lens_symbol` call (with the extracted identifier
/// as its arg), also offers `lens_find` when `pattern` isn't a clean
/// identifier (`lens_symbol` needs a name; `lens_find` takes a free-text query
/// instead), and includes the `ToolSearch` bootstrap line in case the lens
/// tools aren't loaded yet.
pub fn reason(pattern: &str) -> String {
    let trimmed = pattern.trim();
    let (ident, is_clean) = extract_identifier(trimmed);
    let mut out = format!(
        "This grep pattern is a symbol lookup (\"{trimmed}\") — answer it with one lens call instead of a grep chain: lens_symbol(name=\"{ident}\")."
    );
    if !is_clean {
        out.push_str(&format!(
            " \"{trimmed}\" isn't a clean identifier and lens_symbol needs a name — try lens_find(query=\"{trimmed}\") instead."
        ));
    }
    out.push_str(
        " If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_symbol,lens_find,lens_links\"). This fires once per prompt — the same grep will pass if you re-run it.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn def_shape_is_def() {
        assert_eq!(symbol_grep("fn handle_connection"), Some(SymbolKind::Def));
    }

    #[test]
    fn bare_identifier_is_bare() {
        assert_eq!(symbol_grep("TcpListener"), Some(SymbolKind::Bare));
    }

    #[test]
    fn regex_alternation_is_not_a_symbol() {
        assert_eq!(symbol_grep("error|timeout"), None);
    }

    #[test]
    fn short_stopword_is_not_a_symbol() {
        // "the" is len 3 and not a lone keyword, but it's a common English
        // word, not a symbol — the stopword guard keeps it out of Bare.
        assert_eq!(symbol_grep("the"), None);
    }

    #[test]
    fn reason_names_lens_symbol_and_bootstraps_toolsearch() {
        let r = reason("handle_connection");
        assert!(r.contains("lens_symbol"));
        assert!(r.contains("ToolSearch"));
    }
}
