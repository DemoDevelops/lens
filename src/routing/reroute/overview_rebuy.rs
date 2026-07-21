//! overview_rebuy — Rail (ovrb): unfocused lens_overview after SessionStart digest.
//!
//! SessionStart injects a `<repo_map>` digest (empty-seed `overview()`, 1200 tok /
//! 6KB) on `"startup"`. Re-buying the same unfocused map mid-session wastes a
//! round: expand from the digest with `lens_symbol` / `lens_graph` instead.
//! A focused call (`query` set) is not a re-buy — that is a different map.
//!
//! [`is_rebuy`] is the pure classification `route_inner` uses. The digest marker
//! (`ovrb:digest`, written when the digest is actually injected), the once-per-
//! session `ovrb:done` marker (elink pattern: mark BEFORE Deny so the verbatim
//! retry always passes), the rovr stand-down (do not fight the rail that pushes
//! TOWARD lens_overview), and the `LENS_OVERVIEW_REBUY_DENY` gate live in
//! `route_inner` / `session::hook`.

use serde_json::Value;

/// True when this `lens_overview` call is an unfocused re-buy of the digest:
/// no non-empty `query` arg. A focused/seeded overview is a different map and
/// must always pass.
pub fn is_rebuy(tool_input: &Value) -> bool {
    tool_input
        .get("query")
        .and_then(Value::as_str)
        .is_none_or(|s| s.trim().is_empty())
}

/// Deny reason for the ovrb rail: names the SessionStart digest, the expand
/// path via lens_symbol/lens_graph, and the one-shot promise (the `ovrb:done`
/// marker is set before the deny returns, so the verbatim retry always passes).
pub fn deny_reason() -> &'static str {
    "lens routing: the repo map was already injected at session start (<repo_map> digest) — expand any symbol with lens_symbol(name) or lens_graph(node) instead of re-buying the unfocused overview. A focused overview (lens_overview with query) still passes. If you genuinely need the full map again, re-run this call verbatim."
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn empty_and_absent_query_are_rebuy() {
        assert!(is_rebuy(&json!({})));
        assert!(is_rebuy(&json!({"token_budget": 2000})));
        assert!(is_rebuy(&json!({"query": null})));
        assert!(is_rebuy(&json!({"query": ""})));
        assert!(is_rebuy(&json!({"query": "   "})));
    }

    #[test]
    fn non_empty_query_is_not_rebuy() {
        assert!(!is_rebuy(&json!({"query": "auth"})));
        assert!(!is_rebuy(&json!({"query": "routing", "token_budget": 500})));
    }

    #[test]
    fn deny_reason_names_digest_expand_and_retry() {
        let r = deny_reason();
        assert!(r.contains("session start") || r.contains("<repo_map>"), "{r}");
        assert!(r.contains("lens_symbol"), "{r}");
        assert!(r.contains("lens_graph"), "{r}");
        assert!(r.contains("verbatim"), "{r}");
    }
}
