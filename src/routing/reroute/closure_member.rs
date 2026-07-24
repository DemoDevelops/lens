//! closure_member - Rail (cmem): plain walk from a closure member -> deny.
//!
//! The 0060 closure-hint rerun measured the dominant residual lens-arm waste:
//! after holding a `transitive: true` closure (complete, every node with a
//! hop count and call-site witness), sessions re-derived it with depth-1
//! walks on individual members (15 such calls across 2 of 3 sessions, each a
//! full model round). PostToolUse marks every closure member's name and id
//! (`closurenode:{n}`); a later PLAIN `lens_graph` walk (no `to`, not
//! `transitive`) from a marked node is denied once per node per session
//! (`cmem:{n}`, race-safe via `throttle::try_mark`), so the verbatim retry
//! passes. Composed forms never deny. Gate wiring and the
//! `LENS_CLOSURE_MEMBER_DENY` flag live in `route_inner`
//! (`src/routing/mod.rs`), matching the module-boundary convention.

use serde_json::Value;

/// Member identities (each node's `name` and `id`) carried by a `lens_graph`
/// PostToolUse response ONLY when it is a transitive closure
/// (`GraphResponse::Closure`), distinguished from the neighbors shape by the
/// top-level `complete`/`root` fields the closure alone serializes. The
/// neighbors and path shapes, malformed input, and a closure with no `nodes`
/// array all yield an empty list rather than erroring.
pub fn closure_member_names(response: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(response) else {
        return Vec::new();
    };
    if v.get("complete").is_none() || v.get("root").is_none() {
        return Vec::new();
    }
    let Some(nodes) = v.get("nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for n in nodes {
        for key in ["name", "id"] {
            if let Some(s) = n.get(key).and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Deny reason for the cmem rail: names the node, why the walk is redundant
/// (the closure was complete and witnessed), the composed escape, and the
/// once-per-node retry promise.
pub fn deny_reason(node: &str) -> String {
    format!(
        "lens routing: \"{node}\" is already IN the transitive closure this session holds - \
         that response was complete (exhaustive within its depth) and carries this node's hop \
         count and call-site witness, so a fresh walk from it re-derives proven data. Answer \
         from the closure in hand, or compose any residual question in ONE lens_run program \
         (import lens; print only the answer). This fires once per node per session - the \
         verbatim retry passes."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closure_json() -> String {
        serde_json::json!({
            "root": "n0", "root_name": "root_fn", "root_file": "src/root.rs", "root_line": 1,
            "direction": "callers", "depth": 3, "complete": true,
            "count_total": 2, "count_prod": 2,
            "nodes": [
                {"id": "n2", "name": "caller_b", "kind": "function", "file": "src/b.rs", "line": 9, "hops": 2, "witness": "src/b.rs:9"},
                {"id": "n1", "name": "caller_a", "kind": "function", "file": "src/a.rs", "line": 5, "hops": 1, "witness": "src/a.rs:5"},
            ],
        })
        .to_string()
    }

    #[test]
    fn closure_shape_yields_sorted_names_and_ids() {
        assert_eq!(
            closure_member_names(&closure_json()),
            vec!["caller_a", "caller_b", "n1", "n2"]
        );
    }

    #[test]
    fn neighbors_path_and_malformed_shapes_yield_nothing() {
        let neighbors = serde_json::json!({
            "nodes": [{"id": "n1", "name": "caller_a", "kind": "function", "file": "f.rs", "line": 1, "language": "rust"}],
            "edges": [], "truncated": false,
        })
        .to_string();
        assert!(
            closure_member_names(&neighbors).is_empty(),
            "neighbors responses must not arm the rail"
        );
        let path = serde_json::json!({ "found": true, "path": [], "edges": [] }).to_string();
        assert!(closure_member_names(&path).is_empty());
        assert!(closure_member_names("not json").is_empty());
        assert!(closure_member_names("").is_empty());
    }

    #[test]
    fn deny_reason_names_node_escape_and_retry_promise() {
        let r = deny_reason("caller_a");
        assert!(r.contains("caller_a"), "{r}");
        assert!(r.contains("complete"), "{r}");
        assert!(r.contains("lens_run"), "{r}");
        assert!(r.contains("verbatim retry passes"), "{r}");
    }
}
