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
//! threshold, and once-per-session deny (`achain:done`; the counter still
//! resets on fire so the verbatim retry passes, but later chains pass rather
//! than burn a round each - the 2026-07-21 bad-set audit measured repeat
//! denies not converting) plus the `LENS_ATOMIC_CHAIN_DENY` gate live in
//! `route_inner` (`src/routing/mod.rs`), matching the module-boundary
//! convention: pure classification/reason-building here, throttle/gate wiring
//! there.

use serde_json::Value;

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

/// Deny reason for the achain rail, tailored to the denied call itself: the
/// v0.10 gate measured the generic three-shape message converting ~30% (14/46
/// denies followed by lens_run; the rest sidestepped to another atomic tool or
/// retried), so the deny now echoes a copy-pasteable composed call built from
/// the model's own arguments. Falls back to the generic three-shape text when
/// the denied call's args don't pin a shape. Always ends with the retry
/// promise (the deny resets the counter, so the verbatim retry passes).
pub fn deny_reason(tool: &str, input: &Value) -> String {
    const PREAMBLE: &str = "lens routing: 3rd consecutive single-step lens call - chained atomic calls re-enter the model each round, and the rounds, not the tool output, are the token cost. ";
    const GENERIC: &str = "Compose ONE call that answers the whole question instead: multi-hop callers/reachability -> lens_graph {node, transitive: true, direction: \"callers\"} (the complete closure with file:line witnesses in one response); function bodies after a skeleton -> re-call lens_skeleton with include_bodies: [\"name\"] instead of lens_recall.";
    const TAIL: &str = " Any survey/aggregate over files or symbols -> ONE lens_run program (import lens; loop over lens.skeleton/lens.search/lens.callers; print only the final answer). Re-run your call verbatim to proceed.";

    let escape = tailored_escape(tool, input).unwrap_or_else(|| GENERIC.to_string());
    format!("{PREAMBLE}{escape}{TAIL}")
}

/// The chain-specific composed call, built from the denied call's own args so
/// the model can paste it instead of translating generic advice. `None` when
/// the args don't pin a shape (overview, a graph call already composed via
/// `to`/`transitive`, missing fields); the caller falls back to the generic
/// three-shape text.
fn tailored_escape(tool: &str, input: &Value) -> Option<String> {
    match tool {
        "lens_graph" => {
            if input.get("to").is_some()
                || input.get("transitive").is_some_and(|t| t == &Value::Bool(true))
            {
                return None;
            }
            let node = input.get("node")?.as_str()?;
            // transitive closures reject "both"; keep an explicit direction.
            let dir = input
                .get("direction")
                .and_then(Value::as_str)
                .filter(|d| matches!(*d, "callers" | "callees"))
                .unwrap_or("callers");
            Some(format!(
                "Your hop-by-hop walk from \"{node}\" is one closure call: lens_graph {{\"node\": \"{node}\", \"transitive\": true, \"direction\": \"{dir}\", \"depth\": 3}} returns the COMPLETE set within 3 hops, each node with a file:line witness."
            ))
        }
        "lens_symbol" => {
            let name = input.get("name")?.as_str()?;
            Some(format!(
                "Chase \"{name}\"'s connections in one closure call: lens_graph {{\"node\": \"{name}\", \"transitive\": true, \"direction\": \"callers\", \"depth\": 3}} (every transitive caller with a file:line witness)."
            ))
        }
        "lens_skeleton" => {
            let path = input.get("path")?.as_str()?;
            Some(format!(
                "Per-file skeletons re-enter the model once per file - fold the rest into ONE program: lens_run {{\"language\": \"python\", \"code\": \"import lens\\nfor p in [\\\"{path}\\\", <the other files>]:\\n    print(lens.skeleton(p))\"}}. Bodies you need come back verbatim via lens_skeleton {{\"path\": \"{path}\", \"include_bodies\": [\"the_fn\"]}}."
            ))
        }
        "lens_recall" => {
            let ref_hint = input
                .get("ref")
                .and_then(Value::as_str)
                .map_or_else(|| "<ref>".to_string(), |r| format!("\"{r}\""));
            Some(format!(
                "Body-chasing lens_recall burns a round per ref - re-call lens_skeleton with include_bodies: [\"the_fn\"] on the file you skeletonized (bodies come back verbatim), or batch every ref in one lens_run: import lens; [print(lens.recall(r)) for r in [{ref_hint}, ...]]."
            ))
        }
        _ => None,
    }
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
        // lens_overview has no tailored shape, so this exercises the fallback.
        let r = deny_reason("lens_overview", &serde_json::json!({}));
        assert!(r.contains("transitive: true"), "closure escape");
        assert!(r.contains("include_bodies"), "skeleton-bodies escape");
        assert!(r.contains("lens_run"), "composed-program escape");
        assert!(r.contains("import lens"), "prelude composition named");
        assert!(r.contains("verbatim"), "retry promise");
    }

    #[test]
    fn graph_deny_echoes_a_pasteable_closure_call() {
        let r = deny_reason(
            "lens_graph",
            &serde_json::json!({"node": "route_inner", "direction": "callees"}),
        );
        assert!(
            r.contains(r#"lens_graph {"node": "route_inner", "transitive": true, "direction": "callees", "depth": 3}"#),
            "must echo the model's own node and direction: {r}"
        );
        assert!(r.contains("verbatim"), "retry promise survives tailoring");
    }

    #[test]
    fn graph_deny_maps_both_direction_to_callers() {
        // transitive closures reject "both"; the snippet must stay valid.
        let r = deny_reason(
            "lens_graph",
            &serde_json::json!({"node": "x", "direction": "both"}),
        );
        assert!(r.contains(r#""direction": "callers""#), "{r}");
    }

    #[test]
    fn already_composed_graph_calls_fall_back_to_generic() {
        for input in [
            serde_json::json!({"node": "x", "transitive": true, "direction": "callers"}),
            serde_json::json!({"node": "x", "to": "y"}),
            serde_json::json!({}),
        ] {
            let r = deny_reason("lens_graph", &input);
            assert!(
                r.contains("Compose ONE call"),
                "no tailored snippet for {input}: {r}"
            );
        }
    }

    #[test]
    fn skeleton_deny_embeds_the_denied_path() {
        let r = deny_reason(
            "lens_skeleton",
            &serde_json::json!({"path": "src/routing/mod.rs"}),
        );
        assert!(r.contains(r#"lens.skeleton(p)"#), "lens_run loop snippet: {r}");
        assert!(
            r.matches("src/routing/mod.rs").count() >= 2,
            "path echoed in both the loop and the include_bodies escape: {r}"
        );
    }

    #[test]
    fn symbol_deny_routes_to_a_closure_on_the_same_name() {
        let r = deny_reason("lens_symbol", &serde_json::json!({"name": "bump"}));
        assert!(r.contains(r#"lens_graph {"node": "bump", "transitive": true"#), "{r}");
    }

    #[test]
    fn recall_deny_embeds_the_ref_in_the_batch_snippet() {
        let r = deny_reason("lens_recall", &serde_json::json!({"ref": "blob:abc123"}));
        assert!(r.contains(r#""blob:abc123""#), "{r}");
        assert!(r.contains("include_bodies"), "{r}");
    }
}
