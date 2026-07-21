//! atomic_chain — Rail (achain): consecutive single-step lens calls -> compose deny.
//!
//! The 0.11-dev gate run showed the dominant lens-arm cost shape: chains of
//! atomic lens calls (`lens_graph` neighborhood hops, `lens_skeleton` per file,
//! `lens_recall` per elided body) where ONE composed call answers the whole
//! question — task 0060 ran `lens_graph` 8x instead of one
//! `transitive: true` closure; 0068/0070 chased skeleton-elided bodies with
//! repeated `lens_recall` instead of `include_bodies`; 0083 skeletonized six
//! files instead of one `lens_run` program. Each extra round re-enters the
//! model with the full context, so the chain, not the tool output, is the
//! token cost.
//!
//! [`is_atomic`] is the pure classification `route_inner` uses: the
//! exploration tools whose consecutive use marks a chain. `lens_run`,
//! `lens_search`, `lens_grep_ast` and the memory tools BREAK the chain (each
//! is a complete answer, not a hop); every non-lens tool except the
//! `ToolSearch` schema bootstrap breaks it too. The `achain-run` counter,
//! threshold, and per-episode deny (reset on fire, so the verbatim retry
//! passes and a later chain can be denied again — `inspect_escalation`'s
//! "once per drift episode" pattern) plus the `LENS_ATOMIC_CHAIN_DENY` gate
//! live in `route_inner` (`src/routing/mod.rs`), matching the module-boundary
//! convention: pure classification/reason-building here, throttle/gate wiring
//! there.

/// Lens tools whose CONSECUTIVE use marks an atomic exploration chain: each
/// call answers one hop/file/body, and the next hop needs another round.
/// `lens_run` / `lens_search` / `lens_grep_ast` / the memory tools are
/// deliberately absent — each is a complete answer, so it breaks the chain.
/// `name` is the short tool name (`lens_graph`, not `mcp__lens__lens_graph`).
pub fn is_atomic(name: &str) -> bool {
    matches!(
        name,
        "lens_graph" | "lens_skeleton" | "lens_symbol" | "lens_recall" | "lens_overview"
    )
}

/// Deny reason for the achain rail: names the three observed chain shapes and
/// the one composed call that replaces each, plus the retry promise (the deny
/// resets the counter, so the verbatim retry always passes).
pub fn deny_reason() -> &'static str {
    "lens routing: 3rd consecutive single-step lens call - chained atomic calls re-enter the model each round, and the rounds, not the tool output, are the token cost. Compose ONE call that answers the whole question instead: multi-hop callers/reachability -> lens_graph {node, transitive: true, direction: \"callers\"} (the complete closure with file:line witnesses in one response); function bodies after a skeleton -> re-call lens_skeleton with include_bodies: [\"name\"] instead of lens_recall; any survey/aggregate over files or symbols -> ONE lens_run program (import lens; loop over lens.skeleton/lens.search/lens.callers; print only the final answer). Re-run your call verbatim to proceed."
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exploration_tools_are_atomic() {
        for t in [
            "lens_graph",
            "lens_skeleton",
            "lens_symbol",
            "lens_recall",
            "lens_overview",
        ] {
            assert!(is_atomic(t), "{t} must count toward the chain");
        }
    }

    #[test]
    fn complete_answer_tools_break_the_chain() {
        for t in [
            "lens_run",
            "lens_search",
            "lens_grep_ast",
            "lens_memory_query",
            "lens_memory_record",
            "lens_stats",
        ] {
            assert!(!is_atomic(t), "{t} must break the chain");
        }
    }

    #[test]
    fn deny_reason_names_all_three_composed_escapes_and_the_retry() {
        let r = deny_reason();
        assert!(r.contains("transitive: true"), "closure escape");
        assert!(r.contains("include_bodies"), "skeleton-bodies escape");
        assert!(r.contains("lens_run"), "composed-program escape");
        assert!(r.contains("import lens"), "prelude composition named");
        assert!(r.contains("verbatim"), "retry promise");
    }
}
