//! graph_reverify — Rail 3 (grevf): lens_graph answer -> file-reread deny.
//!
//! A `lens_graph` response already carries per-node witnesses (the call-site
//! `file:line` proving each edge) and, for the transitive-closure form, a
//! `complete: true` claim — so once a file has appeared in that answer, going
//! back to re-read/re-grep it a SECOND time is a re-verification of something
//! already proven, not new evidence. [`files_in_graph_response`] is the pure
//! extraction `post_route` uses to mark every file a `lens_graph` call
//! surfaced (`GraphResponse::Neighbors` and `GraphResponse::Closure` both
//! carry a top-level `nodes` array of objects with a `file` field;
//! `GraphResponse::Path`'s `to`-form has no such array at that level and
//! yields nothing, so a shortest-path call never arms this rail). [`deny_reason`]
//! is the one-shot deny arm's message — blocked at most once per FILE per
//! session; the verbatim retry always passes. The per-(session, file)
//! `graphfile:{file}` / `grevf-seen:{file}` / `grevf:{file}` throttle keys and
//! the `LENS_GRAPH_REVERIFY` gate live in `route_inner`/`post_route`
//! (`src/routing/mod.rs`), matching `edit_callers.rs`'s module-boundary
//! convention: pure classification/reason-building here, throttle/gate wiring
//! there.

use serde_json::Value;

/// Distinct file paths named by every node in a `lens_graph` PostToolUse
/// response JSON string. Handles both `GraphResponse::Neighbors` (`GraphView`,
/// a plain neighborhood call — this rail arms on those too, not only
/// transitive closures) and `GraphResponse::Closure` (`TransitiveClosure`):
/// both serialize a top-level `nodes` array whose objects carry a `file`
/// string field. The `to`-form `GraphResponse::Path` has no top-level `nodes`
/// key (it carries `path` instead), so a shortest-path call never arms this
/// rail. Malformed/non-JSON input, or a response with no `nodes` array,
/// yields an empty list rather than erroring.
pub fn files_in_graph_response(response: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(response) else {
        return Vec::new();
    };
    let Some(nodes) = v.get("nodes").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut files: Vec<String> = nodes
        .iter()
        .filter_map(|n| n.get("file").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    files.sort();
    files.dedup();
    files
}

/// Deny reason for the rail's steering arm: names the exact file, the composed
/// `lens.callers(transitive=True)` darkroom program as the preferred next
/// step, and the witness-lines alternative — plus the per-file one-shot
/// promise so the model knows the verbatim retry passes.
pub fn deny_reason(path: &str) -> String {
    format!(
        "{path} already appeared in this session's lens_graph answer, with witness call \
         sites proving every edge and (for a transitive closure) a completeness claim — \
         reading it again to double-check what the graph already told you is a \
         re-verification, not new evidence. Trust the witness lines already in hand, or for \
         a fuller structural answer compose ONE darkroom program instead of a second \
         lens_graph -> Read/Grep round trip: lens_run(language: \"python\", code: \"import \
         lens; r = lens.callers('SYMBOL', transitive=True); print(len(r['nodes'])); \
         print(r['nodes'])\") prints the count and the full witnessed list in a single call. \
         This fires once per file per session — the same Read/Grep will pass if you re-run it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neighbors_shape_yields_distinct_sorted_files() {
        let resp = serde_json::json!({
            "nodes": [
                {"id": "n1", "name": "foo", "kind": "function", "file": "src/b.rs", "line": 1, "language": "rust"},
                {"id": "n2", "name": "bar", "kind": "function", "file": "src/a.rs", "line": 2, "language": "rust"},
                {"id": "n3", "name": "baz", "kind": "function", "file": "src/a.rs", "line": 3, "language": "rust"},
            ],
            "edges": [],
            "truncated": false,
            "resolved": [],
        })
        .to_string();
        assert_eq!(
            files_in_graph_response(&resp),
            vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
        );
    }

    #[test]
    fn closure_shape_yields_files_too() {
        let resp = serde_json::json!({
            "root": "n0", "root_name": "root_fn", "root_file": "src/root.rs", "root_line": 1,
            "direction": "callers", "depth": 2, "complete": true,
            "count_total": 1, "count_prod": 1,
            "nodes": [
                {"id": "n1", "name": "caller", "kind": "function", "file": "src/c.rs", "line": 5, "hops": 1, "witness": "src/c.rs:5"},
            ],
        })
        .to_string();
        assert_eq!(files_in_graph_response(&resp), vec!["src/c.rs".to_string()]);
    }

    #[test]
    fn path_shape_and_malformed_input_yield_nothing() {
        let path_resp = serde_json::json!({
            "found": true,
            "path": [{"id": "n1", "name": "x", "kind": "function", "file": "src/x.rs", "line": 1, "language": "rust"}],
            "edges": [],
            "resolved": [],
        })
        .to_string();
        assert!(
            files_in_graph_response(&path_resp).is_empty(),
            "a shortest-path response has no top-level `nodes` array"
        );
        assert!(files_in_graph_response("not json").is_empty());
        assert!(files_in_graph_response("").is_empty());
    }

    #[test]
    fn deny_reason_names_the_file_and_composed_program_and_one_shot() {
        let r = deny_reason("src/widget.rs");
        assert!(r.contains("src/widget.rs"), "{r}");
        assert!(
            r.contains("lens.callers") && r.contains("transitive=True"),
            "must name the composed program: {r}"
        );
        assert!(r.contains("lens_run"), "{r}");
        assert!(r.contains("once per file per session"), "{r}");
        assert!(r.contains("will pass if you re-run it"), "{r}");
    }
}
