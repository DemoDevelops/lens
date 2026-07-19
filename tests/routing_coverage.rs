//! THE ASSERTION: exhaustive per-tool routing coverage matrix (plan
//! lens-routing-full-closure, T7).
//!
//! [`live_tool_names`] scans the REAL compiled source of `src/server.rs` at
//! test time (`include_str!`, not a hardcoded list) for every `"lens_..."`
//! string literal — the tool names the MCP server actually registers.
//! [`tool_coverage_matrix_is_exhaustive_and_proven`] asserts that set equals
//! [`MATRIX`]'s key set, so a 17th tool added to `server.rs` without a
//! matching matrix entry fails the build here instead of silently losing
//! routing coverage. Every `Covered` entry is then proven live: a full-level
//! `RouteCtx` is built, the canonical misroute call is routed through the
//! real `route()`, and the resulting `Decision` is asserted to deny/modify
//! with a reason naming the tool.

use std::collections::BTreeSet;
use std::path::Path;

use lens::routing::{route, Decision, Level, RouteCtx};
use serde_json::{json, Value};

/// Extract every `"lens_..."`-shaped string literal out of the live source of
/// `src/server.rs` (deduped). A runtime scan of the actual compiled source,
/// not a hardcoded list, so a new tool string added later is picked up
/// automatically and must earn a matrix entry or this test goes red.
fn live_tool_names() -> BTreeSet<String> {
    let src = include_str!("../src/server.rs");
    let mut names = BTreeSet::new();
    let mut rest = src;
    while let Some(pos) = rest.find("\"lens_") {
        let after_quote = &rest[pos + 1..];
        let Some(end) = after_quote.find('"') else {
            break;
        };
        let candidate = &after_quote[..end];
        if candidate.starts_with("lens_")
            && candidate.chars().all(|c| c.is_ascii_lowercase() || c == '_')
        {
            names.insert(candidate.to_string());
        }
        rest = &after_quote[end + 1..];
    }
    names
}

/// How a tool earns its place in the routing surface. Mirrors the plan's
/// four buckets; the non-`Covered` variants carry the rationale for why no
/// deny/modify rail is expected for that tool.
#[allow(dead_code)]
enum Classification {
    /// Reached by a live deny/modify rail — proven below by an inline
    /// `route()` call.
    Covered,
    /// Adoption is inherent to invoking the tool itself (recovering an
    /// offloaded ref, indexing, reading savings stats) — there is no
    /// misroute shape to redirect away from.
    ByConstruction(&'static str),
    /// Surfaced only via the SessionStart guide, never per-call routing; the
    /// standing decision bans new nudge rails for the memory tools.
    SessionSurface(&'static str),
    /// Named as a secondary suggestion inside another tool's own deny
    /// reason (its "host rail"), not the primary target of any rail.
    SecondaryMention(&'static str),
}

/// The full tool-surface matrix. Every key here must equal
/// [`live_tool_names`]'s output exactly, or the coverage assertion fails.
const MATRIX: &[(&str, Classification)] = &[
    (
        "lens_search",
        Classification::Covered, // broad Bash grep deny (also broad Grep-tool deny)
    ),
    ("lens_symbol", Classification::Covered), // def/bare-ident Grep deny (gsym)
    ("lens_grep_ast", Classification::Covered), // syntax-shaped Grep deny (gast)
    ("lens_skeleton", Classification::Covered), // whole unedited code-file Read deny (rskel)
    ("lens_run_file", Classification::Covered), // offset/limit code-file Read deny (runfile)
    ("lens_overview", Classification::Covered), // Nth mapless Read deny (rovr)
    ("lens_run", Classification::Covered),    // data-aggregate Bash pipeline deny (bagg)
    ("lens_links", Classification::Covered),  // decl Edit w/ >=K callers deny (elink)
    (
        "lens_recall",
        Classification::ByConstruction(
            "recovering an offloaded/compacted ref is inherent to using the other tools; \
             there is no misrouted call to redirect toward lens_recall",
        ),
    ),
    (
        "lens_index",
        Classification::ByConstruction(
            "building the index is a one-time setup action, not a response to a misrouted \
             Read/Grep/Bash call",
        ),
    ),
    (
        "lens_stats",
        Classification::ByConstruction(
            "savings telemetry is opt-in introspection, not a target any deny rail redirects \
             a stray tool call toward",
        ),
    ),
    (
        "lens_memory_query",
        Classification::SessionSurface(
            "surfaced via the SessionStart guide only; standing decision bans new nudge rails \
             for the memory tools",
        ),
    ),
    (
        "lens_memory_record",
        Classification::SessionSurface(
            "surfaced via the SessionStart guide only; standing decision bans new nudge rails \
             for the memory tools",
        ),
    ),
    (
        "lens_find",
        Classification::SecondaryMention(
            "named inside the broad-grep deny reason (bash-grep/grep-scope broad deny) as the \
             fallback when the term itself is the unknown",
        ),
    ),
    (
        "lens_path",
        Classification::SecondaryMention(
            "named inside the consecutive-lookup escalation deny (READ_DENY_REASON) as the \
             reachability call — directed per T4's DIRECTED verdict",
        ),
    ),
    (
        "lens_map",
        Classification::SecondaryMention(
            "named inside the read-overview (rovr) deny reason as the prerequisite when the \
             graph hasn't been built yet; no rail targets lens_map on its own",
        ),
    ),
];

/// Seed a populated `index.db` in `data_dir` so `routing::index_present`
/// returns true. Mirrors `tests/routing_tests.rs`'s `seed_index` fixture
/// (the in-crate `#[cfg(test)]` version is unreachable from this
/// integration crate).
fn seed_index(data_dir: &Path) {
    let conn = rusqlite::Connection::open(data_dir.join("index.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_manifest(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);
         INSERT OR REPLACE INTO file_manifest(path, mtime) VALUES ('src/f0.rs', 123);",
    )
    .unwrap();
}

/// Write a `graph.json` fixture, same node/edge shape as
/// `tests/routing_tests.rs`'s `callers_graph_json`/`resolving_graph_json`.
fn write_graph(data_dir: &Path, graph: Value) {
    std::fs::write(data_dir.join("graph.json"), graph.to_string()).unwrap();
}

/// A full-level, MCP-ready `RouteCtx` for one scenario. Each call site passes
/// a distinct `session` so the in-memory throttle never leaks state between
/// matrix entries, mirroring `src/routing/mod.rs`'s own `rc()` test helper.
fn full_ctx<'a>(data_dir: &'a Path, session: &'a str, reads_since_map: u64) -> RouteCtx<'a> {
    RouteCtx {
        level: Level::Full,
        mcp_ready: true,
        bin: "/path/to/lens",
        data_dir,
        session_id: session,
        rtk_active: false,
        reads_since_map,
    }
}

/// Route `tool`/`input` through `ctx` and assert the verdict denies (or
/// modifies) with a reason containing `needle`. Returns the reason so a
/// caller can assert a second, secondary-mention substring on the same call
/// without re-routing it.
fn assert_deny_or_modify_containing(tool: &str, input: &Value, ctx: &RouteCtx, needle: &str) -> String {
    match route(tool, input, ctx) {
        Decision::Deny(reason) => {
            assert!(
                reason.contains(needle),
                "{tool} misroute must deny toward {needle:?}: {reason}"
            );
            reason
        }
        Decision::Modify { reason, .. } => {
            assert!(
                reason.contains(needle),
                "{tool} misroute must modify toward {needle:?}: {reason}"
            );
            reason
        }
        other => panic!("expected {tool} misroute to deny/modify toward {needle:?}, got {other:?}"),
    }
}

#[test]
fn tool_coverage_matrix_is_exhaustive_and_proven() {
    // ── The assertion: every live lens_* tool has a classification, and vice versa ──
    let live = live_tool_names();
    let matrix_keys: BTreeSet<String> = MATRIX.iter().map(|(name, _)| name.to_string()).collect();
    assert_eq!(
        live, matrix_keys,
        "every lens_* tool string in src/server.rs must have a routing-coverage \
         classification in MATRIX (and vice versa) — live tools: {live:?}, matrix keys: {matrix_keys:?}"
    );

    // ── lens_search: broad Bash grep denies toward lens_search, and mentions
    //    lens_find as a secondary (SecondaryMention proof). The bash-grep arm
    //    shares the Grep tool's per-prompt deny budget (armed at
    //    UserPromptSubmit in production, see `session::hook`) — arm it here
    //    the same way (`grep-scope`, bumped on every steering prompt). ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-search", 0);
        lens::routing::throttle::bump(d.path(), "cov-search", "grep-scope");
        let input = json!({"command": "grep -rn foo src/"});
        let reason = assert_deny_or_modify_containing("Bash", &input, &ctx, "lens_search");
        assert!(
            reason.contains("lens_find"),
            "broad-grep deny (lens_search's host rail) must also mention lens_find: {reason}"
        );
    }

    // ── lens_symbol: a def-shaped Grep pattern that resolves in the graph
    //    denies toward lens_symbol (gsym) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        write_graph(
            d.path(),
            json!({
                "nodes": [
                    {"id": "n1", "name": "parse_grep_seg", "kind": "function", "file": "src/a.rs", "line": 1, "language": "rust"},
                ],
                "edges": [],
            }),
        );
        let ctx = full_ctx(d.path(), "cov-symbol", 0);
        let input = json!({"pattern": "fn parse_grep_seg"});
        assert_deny_or_modify_containing("Grep", &input, &ctx, "lens_symbol");
    }

    // ── lens_grep_ast: a syntax-shaped (not symbol-shaped) Grep pattern
    //    denies toward lens_grep_ast (gast) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-ast", 0);
        let input = json!({"pattern": ".clone()"});
        assert_deny_or_modify_containing("Grep", &input, &ctx, "lens_grep_ast");
    }

    // ── lens_skeleton: a whole, unedited code-file Read denies toward
    //    lens_skeleton (rskel) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-skeleton", 0);
        let input = json!({"file_path": "src/whatever.rs"});
        assert_deny_or_modify_containing("Read", &input, &ctx, "lens_skeleton");
    }

    // ── lens_run_file: a bounded (offset/limit) code-file Read denies toward
    //    lens_run_file (runfile) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-runfile", 0);
        let input = json!({"file_path": "src/whatever.rs", "offset": 10, "limit": 50});
        assert_deny_or_modify_containing("Read", &input, &ctx, "lens_run_file");
    }

    // ── lens_overview: the Nth Read this session with no lens_map/lens_overview
    //    call yet denies toward lens_overview (rovr) — and mentions lens_map as
    //    the prerequisite (SecondaryMention proof). A non-code path (no
    //    CODE_EXTENSIONS match) so the higher-priority rskel arm never fires
    //    first and masks rovr. ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-overview", 5); // at the default rovr threshold
        let input = json!({"file_path": "README.md"});
        let reason = assert_deny_or_modify_containing("Read", &input, &ctx, "lens_overview");
        assert!(
            reason.contains("lens_map"),
            "rovr deny (lens_overview's host rail) must also mention lens_map: {reason}"
        );
    }

    // ── lens_run: a data-aggregate Bash pipeline denies toward lens_run (bagg) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-run", 0);
        let input = json!({"command": "find . -name '*.rs' | wc -l"});
        assert_deny_or_modify_containing("Bash", &input, &ctx, "lens_run");
    }

    // ── lens_links: an Edit touching a declaration with >= min_callers
    //    callers denies toward lens_links (elink) ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        write_graph(
            d.path(),
            json!({
                "nodes": [
                    {"id": "a1", "name": "alpha", "kind": "function", "file": "src/a.rs", "line": 1, "language": "rust"},
                    {"id": "c1", "name": "caller_one", "kind": "function", "file": "src/c.rs", "line": 1, "language": "rust"},
                    {"id": "c2", "name": "caller_two", "kind": "function", "file": "src/c.rs", "line": 10, "language": "rust"},
                    {"id": "c3", "name": "caller_three", "kind": "function", "file": "src/c.rs", "line": 20, "language": "rust"},
                ],
                "edges": [
                    {"from": "c1", "to": "a1", "kind": "calls"},
                    {"from": "c2", "to": "a1", "kind": "calls"},
                    {"from": "c3", "to": "a1", "kind": "calls"},
                ],
            }),
        );
        let ctx = full_ctx(d.path(), "cov-links", 0);
        let input = json!({
            "file_path": "src/a.rs",
            "old_string": "fn alpha(a: i32)",
            "new_string": "fn alpha(a: i64)",
        });
        assert_deny_or_modify_containing("Edit", &input, &ctx, "lens_links");
    }

    // ── lens_path (SecondaryMention): the consecutive-lookup escalation deny
    //    fires on the 4th consecutive plain-text Grep with no lens tool call
    //    in between, and names lens_path as the reachability call ──
    {
        let d = tempfile::tempdir().unwrap();
        let ctx = full_ctx(d.path(), "cov-escalation", 0);
        let input = json!({"pattern": "hello world"}); // neither symbol- nor ast-shaped
        for _ in 0..3 {
            assert_eq!(
                route("Grep", &input, &ctx),
                Decision::Passthrough,
                "lookup counter must build without denying below the escalation threshold"
            );
        }
        match route("Grep", &input, &ctx) {
            Decision::Deny(reason) => assert!(
                reason.contains("lens_path"),
                "the escalation deny (lens_path's host rail) must mention lens_path: {reason}"
            ),
            other => panic!("expected the escalation deny on the 4th consecutive lookup, got {other:?}"),
        }
    }
}
