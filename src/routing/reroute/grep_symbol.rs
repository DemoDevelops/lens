//! grep_symbol: Grep(symbol) → lens_symbol classifier.
//!
//! Rail 1a of the reroute family (see [`super`] for the counter-key/env-flag
//! contract). [`symbol_grep`] recognizes a Grep `pattern` that is really a
//! symbol lookup — a definition shape (`fn foo`, `class Bar`, …) or a bare
//! identifier — the case the graph answers directly instead of a grep→Read
//! chain. [`deny_reason`] renders the one-shot deny text attached to the
//! `Decision::Deny`, naming the exact lens call (with args) rather than a
//! generic tool list — mirrors the shape of `GREP_FIRST_DENY_REASON`
//! (`src/routing/mod.rs`: state the trigger, name the call, offer the
//! `ToolSearch` bootstrap, note it's one-shot) but keyed on the call's pattern
//! instead of the prompt's phrasing. [`nudge`] is the same guidance phrased as
//! a soft suggestion for the rail's `Level::Nudge` arm.

use std::path::Path;
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
/// it is a clean identifier (so an exact name match is likely) or a fallback
/// (so [`deny_reason`] notes that `lens_symbol` still works via its
/// meaning-match fallback when the text isn't a clean name).
fn extract_identifier(trimmed: &str) -> (String, bool) {
    if let Some(name) = def_re().captures(trimmed).and_then(|c| c.get(1)) {
        return (name.as_str().to_string(), true);
    }
    if bare_re().is_match(trimmed) {
        return (trimmed.to_string(), true);
    }
    (trimmed.to_string(), false)
}

/// True when `pat`'s extracted identifier resolves to a real graph node —
/// i.e. `lens_symbol(name=ident)` would find something rather than coming up
/// empty. Gates the gsym deny (`route_inner`'s `grep_symbol_deny_enabled`
/// chain in `src/routing/mod.rs`) so it never redirects a Grep toward a lens
/// lookup that dead-ends. A missing or unreadable `graph.json` (no graph
/// built yet) is a NO — no architecture to check against means no deny.
pub fn graph_resolves(data_dir: &Path, pat: &str) -> bool {
    if symbol_grep(pat).is_none() {
        return false;
    }
    let (ident, _) = extract_identifier(pat.trim());
    let Ok(raw) = std::fs::read_to_string(data_dir.join("graph.json")) else {
        return false;
    };
    name_value_contains(&raw, &ident)
}

/// Scan raw `graph.json` text for a `"name":"..."` value that contains
/// `ident` case-insensitively, mirroring `Graph::find_by_name`'s substring
/// semantics — without a serde parse of the (multi-MB) file. Walks byte
/// occurrences of `"name":"` and reads each value to its closing UNESCAPED
/// quote.
fn name_value_contains(raw: &str, ident: &str) -> bool {
    let ident_lower = ident.to_ascii_lowercase();
    let bytes = raw.as_bytes();
    let needle = b"\"name\":\"";
    let mut start = 0;
    while start < bytes.len() {
        let Some(rel) = bytes[start..]
            .windows(needle.len())
            .position(|w| w == needle)
        else {
            return false;
        };
        let value_start = start + rel + needle.len();
        let mut end = value_start;
        while end < bytes.len() && !(bytes[end] == b'"' && bytes[end - 1] != b'\\') {
            end += 1;
        }
        if raw[value_start..end.min(bytes.len())]
            .to_ascii_lowercase()
            .contains(&ident_lower)
        {
            return true;
        }
        start = end + 1;
    }
    false
}

/// Deny reason for a Grep `pattern` [`symbol_grep`] identified as a symbol
/// lookup: names the exact `lens_symbol` call (with the extracted identifier
/// as its arg), notes `lens_symbol`'s own meaning-match fallback when
/// `pattern` isn't a clean identifier, and includes the `ToolSearch`
/// bootstrap line in case the lens tools aren't loaded yet.
pub fn deny_reason(pattern: &str) -> String {
    let trimmed = pattern.trim();
    let (ident, is_clean) = extract_identifier(trimmed);
    let mut out = format!(
        "This grep pattern is a symbol lookup (\"{trimmed}\") — answer it with one lens call instead of a grep chain: lens_symbol(name=\"{ident}\")."
    );
    if !is_clean {
        out.push_str(&format!(
            " \"{trimmed}\" isn't a clean identifier, but lens_symbol still resolves it: it falls back to a meaning match when nothing matches by name."
        ));
    }
    out.push_str(
        " If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_symbol\"). This fires once per prompt — the same grep will pass if you re-run it.",
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
    fn deny_reason_names_lens_symbol_and_bootstraps_toolsearch() {
        let r = deny_reason("handle_connection");
        assert!(r.contains("lens_symbol"));
        assert!(r.contains("ToolSearch"));
    }

    fn write_graph(dir: &std::path::Path, name: &str) {
        std::fs::write(
            dir.join("graph.json"),
            format!(
                r#"{{"nodes":[{{"id":"n1","name":"{name}","kind":"function","file":"a.rs","line":1,"language":"rust"}}],"edges":[]}}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn graph_resolves_hits_def_and_bare_pattern() {
        let dir = tempfile::tempdir().unwrap();
        write_graph(dir.path(), "known_name");
        assert!(graph_resolves(dir.path(), "fn known_name"));
        assert!(graph_resolves(dir.path(), "known_name"));
    }

    #[test]
    fn graph_resolves_false_on_miss() {
        let dir = tempfile::tempdir().unwrap();
        write_graph(dir.path(), "other_name");
        assert!(!graph_resolves(dir.path(), "known_name"));
    }

    #[test]
    fn graph_resolves_false_without_graph_file() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!graph_resolves(dir.path(), "known_name"));
    }

    #[test]
    fn graph_resolves_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        write_graph(dir.path(), "KNOWN_NAME");
        assert!(graph_resolves(dir.path(), "known_name"));
    }
}
