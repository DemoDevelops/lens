//! edit_callers — Rail 2a: Edit(symbol with >=K callers) -> lens_links nudge.
//!
//! When an Edit touches a *declaration* line — a `fn`/`def`/`func`/`class`/
//! `struct` signature — changing that signature can break every caller. We look
//! the symbol up in the structural graph and, if it has at least K incoming
//! `calls` edges (callers), route the editor at `lens_links` so the blast
//! radius is visible before committing: [`caller_nudge`] is the
//! `Decision::Context` arm, [`deny_reason`] the one-shot deny arm (blocked at
//! most once per symbol per session; the verbatim retry always passes). Pure
//! and graph-backed; the once-per-(session, symbol) `elink:{sym}` throttle and
//! the `LENS_EDIT_LINKS_NUDGE`/`LENS_EDIT_LINKS_DENY` gates live in
//! `route_inner` (`src/routing/mod.rs`).
//!
//! Caller-edge direction: a `calls` edge is stored `from` = caller, `to` =
//! callee (see [`crate::discovery::graph`]), so the callers of `sym` are the
//! edges whose `to` resolves to a node named `sym`.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;

use crate::discovery::graph::Graph;

/// Declaration matcher: an optional `pub` then a `fn`/`def`/`func`/`class`/
/// `struct` keyword followed by the declared name. Group 1 is the symbol name.
/// The keyword set is deliberately narrow (callable / type declarations) — the
/// broader symbol set lives in `grep_symbol`.
static DECL_RE: OnceLock<Regex> = OnceLock::new();

fn decl_re() -> &'static Regex {
    DECL_RE.get_or_init(|| {
        Regex::new(r"\b(?:pub\s+)?(?:fn|def|func|class|struct)\b\s+(\w+)").expect("decl regex")
    })
}

/// The symbol whose *declaration* this edit touches, or `None` for a body-only
/// edit. Signature-touching edits are the caller-breaking case, so we fire only
/// when `old_string` carries a declaration token, returning the FIRST declared
/// name. `new_string` is accepted for the T8 call contract but the
/// decl-in-`old_string` presence is the rule, so it is intentionally unused.
pub fn edited_symbol(old_string: &str, _new_string: &str) -> Option<String> {
    decl_re().captures(old_string).map(|c| c[1].to_string())
}

/// Number of incoming `calls` edges (callers) for `sym`, or `None` when `sym`
/// is absent from the graph (distinct from an in-graph 0-caller symbol). A
/// `calls` edge is stored `from` = caller, `to` = callee, so the callers are
/// exactly the `calls` edges whose `to` resolves to a node named `sym` (there
/// may be several such nodes — same name in different files/types).
pub fn caller_count(graph: &Graph, sym: &str) -> Option<usize> {
    let target_ids: HashSet<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.name == sym)
        .map(|n| n.id.as_str())
        .collect();
    if target_ids.is_empty() {
        return None;
    }
    Some(
        graph
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && target_ids.contains(e.to.as_str()))
            .count(),
    )
}

/// Nudge toward `lens_links` when `sym` has at least `k` incoming `calls` edges
/// (callers). Returns `None` when `sym` is absent from the graph or has fewer
/// than `k` callers (see [`caller_count`]).
pub fn caller_nudge(graph: &Graph, sym: &str, k: usize) -> Option<String> {
    let count = caller_count(graph, sym).filter(|&count| count >= k)?;
    Some(format!(
        "`{sym}` has {count} callers — run lens_links(\"{sym}\") before you change its \
         signature. If the lens tools aren't loaded yet, load them first: \
         ToolSearch(query: \"select:lens_links,lens_path\")."
    ))
}

/// Deny reason for the rail's steering arm — the same blast-radius guidance as
/// [`caller_nudge`] with the real caller count, plus the one-shot promise: the
/// `elink:{sym}` marker is set before the deny returns, so an Edit is blocked
/// at most once per symbol per session and the verbatim retry always passes.
pub fn deny_reason(sym: &str, callers: usize) -> String {
    format!(
        "`{sym}` has {callers} callers — its declaration is about to change, so see the \
         blast radius first: lens_links(\"{sym}\") lists every caller in one call. If the \
         lens tools aren't loaded yet, load them first: \
         ToolSearch(query: \"select:lens_links,lens_path\"). This fires at most once per \
         symbol per session — the same Edit will pass if you re-run it verbatim."
    )
}

/// Minimum caller count that arms the nudge. `LENS_EDIT_LINKS_MIN_CALLERS`
/// overrides it; absence or a parse failure falls back to 3.
pub fn min_callers() -> usize {
    std::env::var("LENS_EDIT_LINKS_MIN_CALLERS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::graph::{Graph, Node};

    /// A graph where `foo` has `callers` distinct incoming `calls` edges.
    fn graph_with_callers(callers: usize) -> Graph {
        let mut g = Graph::new();
        let foo = g.add_node(Node::new("f.rs", "function", "foo", 1, "rust"));
        for i in 0..callers {
            let c = g.add_node(Node::new(
                "callers.rs",
                "function",
                &format!("caller{i}"),
                (i + 1) * 10,
                "rust",
            ));
            // Edge points from the caller to the callee (`foo`).
            g.add_edge(&c, &foo, "calls");
        }
        g
    }

    #[test]
    fn decl_edit_to_multicaller_symbol_nudges() {
        // A decl-touching edit: old_string carries the `fn foo(...)` signature.
        assert_eq!(
            edited_symbol("fn foo(a: i32) -> i32", "fn foo(a: i64) -> i64").as_deref(),
            Some("foo")
        );
        let g = graph_with_callers(4);
        let nudge = caller_nudge(&g, "foo", 3).expect("4 callers >= k=3 must nudge");
        assert!(
            nudge.contains('4'),
            "must embed the real caller count: {nudge}"
        );
        assert!(
            nudge.contains("lens_links"),
            "must name lens_links: {nudge}"
        );
        assert!(nudge.contains("foo"), "must name the symbol: {nudge}");
    }

    #[test]
    fn body_only_edit_extracts_no_symbol() {
        // No fn/def/func/class/struct declaration token => not caller-breaking.
        assert_eq!(
            edited_symbol(
                "    let total = items.iter().sum::<u32>();",
                "    let total = items.len() as u32;"
            ),
            None
        );
    }

    #[test]
    fn single_caller_below_threshold_is_silent() {
        let g = graph_with_callers(1);
        assert_eq!(caller_nudge(&g, "foo", 3), None);
    }

    #[test]
    fn pub_fn_signature_edit_names_the_symbol() {
        assert_eq!(
            edited_symbol("pub fn handle(x: i32)", "pub fn handle(x: i64)").as_deref(),
            Some("handle")
        );
    }

    #[test]
    fn symbol_absent_from_graph_is_silent() {
        // `sym` isn't in the graph => None (distinct from an in-graph 0-caller).
        let g = graph_with_callers(4);
        assert_eq!(caller_nudge(&g, "missing", 3), None);
        assert_eq!(caller_count(&g, "missing"), None);
    }

    #[test]
    fn caller_count_reports_the_exact_count() {
        let g = graph_with_callers(4);
        assert_eq!(caller_count(&g, "foo"), Some(4));
    }

    #[test]
    fn deny_reason_names_symbol_count_and_retry_promise() {
        let r = deny_reason("foo", 4);
        assert!(r.contains("lens_links(\"foo\")"), "{r}");
        assert!(r.contains("4 callers"), "{r}");
        assert!(r.contains("once per symbol per session"), "{r}");
        assert!(r.contains("will pass if you re-run it verbatim"), "{r}");
    }

    #[test]
    fn count_equal_to_threshold_fires() {
        // count == k is on the fire side of the `>= k` gate.
        let g = graph_with_callers(3);
        assert!(caller_nudge(&g, "foo", 3).is_some());
    }

    #[test]
    fn min_callers_defaults_to_three_when_unset() {
        // Read-only: assert the default only when the override is absent, to stay
        // safe under parallel test execution (no global env mutation here).
        if std::env::var_os("LENS_EDIT_LINKS_MIN_CALLERS").is_none() {
            assert_eq!(min_callers(), 3);
        }
    }
}
