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

use lens::routing::{post_route, route, session_block, Decision, Level, RouteCtx};
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
    ("lens_overview", Classification::Covered), // Nth mapless Read deny (rovr)
    // 0.10.0 fold: lens_run absorbed lens_run_file, so it hosts both the
    // data-aggregate Bash pipeline deny (bagg) and the offset/limit code-file
    // Read deny (runfile).
    ("lens_run", Classification::Covered),
    // 0.10.0 fold: lens_graph absorbed lens_links + lens_path, so it hosts the
    // decl-Edit >=K-callers deny (elink) and the consecutive-lookup escalation
    // deny (the rails' messages are renamed by the T6 sweep; the proofs below
    // accept either spelling so the two wave-2 tasks can land in any order).
    ("lens_graph", Classification::Covered),
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
            "no longer an MCP tool (0.10.0 fold): the literal is ensure_index's auto-build \
             op label; building the index is inherent to the query path",
        ),
    ),
    (
        "lens_map",
        Classification::ByConstruction(
            "no longer an MCP tool (0.10.0 fold): the literal is ensure_graph's auto-build \
             op label; building the graph is inherent to the query path",
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
        // T6 landed: the secondary suggestion is lens_symbol (whose fallback
        // absorbed lens_find). Pinned to the folded name only, so a revert to
        // the removed tool name fails here.
        assert!(
            reason.contains("lens_symbol"),
            "broad-grep deny must also mention the by-meaning fallback: {reason}"
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

    // ── lens_run (runfile rail): a bounded (offset/limit) code-file Read denies
    //    toward the darkroom file-analysis verb. WAVE-2 SEAM: "lens_run" is a
    //    prefix of the pre-T6 "lens_run_file" spelling, so this needle passes
    //    before and after the T6 rename sweep. ──
    {
        let d = tempfile::tempdir().unwrap();
        seed_index(d.path());
        let ctx = full_ctx(d.path(), "cov-runfile", 0);
        let input = json!({"file_path": "src/whatever.rs", "offset": 10, "limit": 50});
        assert_deny_or_modify_containing("Read", &input, &ctx, "lens_run");
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
        // WAVE-2 SEAM: the pre-T6 rovr message also names lens_map as the
        // prerequisite; post-fold the graph auto-builds and the T6 sweep drops
        // that mention, so only the primary lens_overview needle is pinned here.
        assert_deny_or_modify_containing("Read", &input, &ctx, "lens_overview");
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
        // T6 landed: the elink rail names lens_graph. Pinned to the folded
        // name only, so a revert to the removed tool name fails here.
        let reason = assert_deny_or_modify_containing("Edit", &input, &ctx, "lens_");
        assert!(
            reason.contains("lens_graph"),
            "elink deny must name the neighborhood verb: {reason}"
        );
    }

    // ── lens_graph (escalation rail): the consecutive-lookup escalation deny
    //    fires on the 4th consecutive plain-text Grep with no lens tool call
    //    in between, and names the reachability verb. WAVE-2 SEAM: pre-T6 that
    //    is lens_path; the T6 sweep retargets it to lens_graph. ──
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
                reason.contains("lens_graph"),
                "the escalation deny must mention the reachability verb: {reason}"
            ),
            other => panic!("expected the escalation deny on the 4th consecutive lookup, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// T4 (graph_reverify): a `lens_graph` PostToolUse response marks its files;
// route()'s Read/Grep arms deny that file's 2nd+ visit toward the composed
// `lens.callers(transitive=True)` darkroom program.
// ---------------------------------------------------------------------------

/// A minimal `GraphResponse::Neighbors` (`GraphView`)-shaped JSON string
/// naming `file` on its one node — enough for
/// `reroute::graph_reverify::files_in_graph_response` to extract it.
fn graph_view_response(file: &str) -> String {
    json!({
        "nodes": [
            {"id": "n1", "name": "some_symbol", "kind": "function", "file": file, "line": 1, "language": "rust"},
        ],
        "edges": [],
        "truncated": false,
        "resolved": [],
    })
    .to_string()
}

// Both env-independent assertions AND the `LENS_GRAPH_REVERIFY=0` kill-switch
// live in ONE test function: `cargo test` runs different `#[test]` fns on
// separate threads in the same process, so a second test flipping the shared
// process-global env var mid-run could race this one — folding the kill
// switch in here (env restored before returning) is the same one-test
// discipline used for env-mutating cases elsewhere in this suite.
#[test]
fn graph_reverify_denies_the_second_visit_to_a_graph_surfaced_file() {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    let ctx = full_ctx(d.path(), "cov-grevf", 0);

    // A lens_graph PostToolUse call surfaces src/widget.rs as a node — never
    // denies (PostToolUse can only observe), but arms the file marker.
    assert_eq!(
        post_route(
            "mcp__lens__lens_graph",
            &graph_view_response("src/widget.rs"),
            &ctx,
        ),
        Decision::Passthrough,
    );

    // First Grep of that exact file passes (the agent still needs one look);
    // a narrow single-file scope with a plain-text pattern also keeps every
    // OTHER Grep rail (scope/gsym/gast) out of the way, so grevf is the only
    // actor here.
    let grep_widget = json!({"pattern": "some_symbol_use", "path": "src/widget.rs"});
    assert_eq!(
        route("Grep", &grep_widget, &ctx),
        Decision::Passthrough,
        "the first visit to a graph-surfaced file must pass"
    );

    // Second visit denies, naming the file and the composed program.
    let reason = assert_deny_or_modify_containing("Grep", &grep_widget, &ctx, "src/widget.rs");
    assert!(
        reason.contains("lens.callers") && reason.contains("transitive=True"),
        "deny must name the composed program: {reason}"
    );

    // One-shot per file: the verbatim retry (3rd visit) passes.
    assert_eq!(
        route("Grep", &grep_widget, &ctx),
        Decision::Passthrough,
        "the verbatim retry must pass — denied at most once per file per session"
    );

    // A DIFFERENT graph-surfaced file gets its own one-shot, proven via Read
    // this time (the rail applies to both tools, keyed on the same
    // `graphfile:{path}`/`grevf:{path}` markers regardless of which one asks).
    // A whole-file Read also happens to be rskel-eligible, but grevf runs
    // FIRST in `read_decision`, so its own verdict — pass, then deny with the
    // composed-program reason — wins outright on the file's 1st/2nd visits.
    let ctx2 = full_ctx(d.path(), "cov-grevf-2", 0);
    post_route(
        "mcp__lens__lens_graph",
        &graph_view_response("src/other.rs"),
        &ctx2,
    );
    let read_other = json!({"file_path": "src/other.rs"});
    // 1st visit: grevf lets it through (still passes to whichever OTHER rail
    // wants it, e.g. rskel — irrelevant here, just not a grevf deny).
    match route("Read", &read_other, &ctx2) {
        Decision::Deny(reason) => assert!(
            !reason.contains("lens.callers"),
            "the FIRST visit must not be the grevf deny: {reason}"
        ),
        Decision::Passthrough => {}
        other => panic!("unexpected first-visit decision: {other:?}"),
    }
    // 2nd visit: grevf wins outright with its own composed-program reason.
    match route("Read", &read_other, &ctx2) {
        Decision::Deny(reason) => assert!(
            reason.contains("src/other.rs") && reason.contains("lens.callers"),
            "the file's 2nd visit must be the grevf deny: {reason}"
        ),
        other => panic!("expected the grevf deny on this file's 2nd visit, got {other:?}"),
    }

    // A file never surfaced by any lens_graph call is untouched by this rail.
    let ctx3 = full_ctx(d.path(), "cov-grevf-3", 0);
    let never_surfaced = json!({"pattern": "some_symbol_use", "path": "src/never_surfaced.rs"});
    for _ in 0..3 {
        assert_eq!(
            route("Grep", &never_surfaced, &ctx3),
            Decision::Passthrough,
            "a file never surfaced by lens_graph must never grevf-deny"
        );
    }

    // Kill switch: LENS_GRAPH_REVERIFY=0 disables the rail outright, even for
    // a file the graph did surface.
    std::env::set_var("LENS_GRAPH_REVERIFY", "0");
    let ctx4 = full_ctx(d.path(), "cov-grevf-off", 0);
    post_route(
        "mcp__lens__lens_graph",
        &graph_view_response("src/killswitch.rs"),
        &ctx4,
    );
    let grep_killswitch = json!({"pattern": "some_symbol_use", "path": "src/killswitch.rs"});
    for i in 1..=3 {
        assert_eq!(
            route("Grep", &grep_killswitch, &ctx4),
            Decision::Passthrough,
            "visit {i}: LENS_GRAPH_REVERIFY=0 must never deny"
        );
    }
    std::env::remove_var("LENS_GRAPH_REVERIFY");
}

#[test]
fn session_start_guide_carries_the_composed_program_worked_example_exactly_once() {
    let b = session_block(Level::Full);
    let needle = "compose it in the darkroom instead of firing lens_graph repeatedly";
    assert!(
        b.contains(needle),
        "the guide must carry the composed-program worked example: {b}"
    );
    assert_eq!(
        b.matches(needle).count(),
        1,
        "the worked example must appear exactly once in the assembled guide"
    );
    // The composed call itself, and its transitive-closure flag, are part of
    // that same worked example.
    assert!(b.contains("lens.callers('X', transitive=True"));
}
