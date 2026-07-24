//! T8: dogfood replay regression (plan `lens-routing-full-closure.md`).
//!
//! Two fixtures, both driving the REAL compiled binary the same way
//! `tests/routing_tests.rs` does (`lens hook claude <event>` over stdin —
//! `route()`/`RouteCtx` are only reachable in-process via the in-crate
//! `#[cfg(test)]` seams, which this integration crate can't see):
//!
//!   * `tests/fixtures/routing_replay.jsonl` — derived from the 162-event
//!     `.lens/routing.log` captured during the 2026-07-19 dogfood audit
//!     (source: `/Users/gene/Documents/AI Stuff/lens/.lens/routing.log`,
//!     read-only, never modified). The log schema (`session`/`tool`/`cmd`/
//!     `decision`/`reason`) only serializes a real `tool_input` for `Bash`
//!     (the `cmd` field); every other tool's original `pattern`/`file_path`
//!     was never captured. This fixture keeps only the subset honestly
//!     reconstructable from what the log actually recorded: the 7 real Bash
//!     commands, the 82 `mcp__lens__*`/`ToolSearch` events (lens's own tool
//!     calls, trivially and permanently `passthrough` — route_inner has no
//!     match arm for them), and the 18 Grep `grep-symbol` denies whose exact
//!     pattern is quoted verbatim inside the logged deny reason (e.g. `This
//!     grep pattern is a symbol lookup ("fn doctor")`). The remaining 55
//!     events (escalation-gated denies needing multi-call session history,
//!     and generic-reason nudge/passthrough entries with zero recoverable
//!     specifics) are not in this fixture — fabricating their original call
//!     shape would not be a real replay. 107 >= the 100-event predicate.
//!
//!   * `tests/fixtures/mined_replay.jsonl` — the 98-case mined corpus
//!     (`benchmarks/mined/cases.jsonl` in the main checkout), copied with
//!     `meta` dropped. 12 `GA-FOLLOWUP` cases (a grep/Bash/Read fallback
//!     after `lens_grep_ast` failed to parse a variadic/fragment/S-expr
//!     pattern) are quarantined: the grep_ast DSL parser work is explicitly
//!     out of scope for this plan, so denying the fallback back toward a
//!     tool that will fail the same way again isn't a verdict this test can
//!     honestly pin. Every other case is verdict-asserted. For 29 of the 86
//!     non-quarantined cases the corpus's original `expect` (computed before
//!     this plan's H0-H6/T14 changes landed) disagreed with the actual,
//!     current, and INTENTIONAL routing behavior: every one of the 29 is a
//!     Bash/Grep/Read call whose target is a single concrete file (or, for
//!     two Bash cases, a lead segment `bash_grep`'s parser doesn't recognize
//!     as a grep invocation at all) — the deliberate scoped-escape design
//!     ("Keep the scoped-grep escape open: single-file/exact-string greps
//!     stay passthrough", plan Context) that this same plan's H1 explicitly
//!     preserves. This fixture's `expect` field for those 29 was corrected
//!     to the verified, current `passthrough` rather than blindly replaying
//!     the corpus's stale `rewrite`/`deny` guess — see `generate_fixtures.py`
//!     history for the ground-truth derivation. Flagged for review: this is
//!     a judgment call about 29 specific lines, not a code change.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_lens")
}

/// The fixtures were mined on the machine whose checkout lived at this prefix.
/// `grep_scope` (src/routing/classify.rs) stats a Grep `path` input: a real
/// directory is Broad (deniable) but ENOENT is Unknown (passthrough bias), so
/// replaying the recorded absolute paths on another machine (CI) flips deny
/// verdicts to passthrough. Rewrite the recorded checkout prefix to this
/// checkout's root before replaying; on the mining machine this is a no-op.
const MINED_CHECKOUT: &str = "/Users/gene/Documents/AI Stuff/lens/";

fn localize(input: &Value) -> Value {
    let here = format!("{}/", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&input.to_string().replace(MINED_CHECKOUT, &here)).unwrap()
}

/// Seed a populated `index.db` in `data_dir` so `routing::index_present`
/// returns true — mirrors `routing_tests.rs`'s own `seed_index` (the in-crate
/// `#[cfg(test)]` fixture is unreachable from this integration crate).
fn seed_index(data_dir: &Path) {
    let conn = rusqlite::Connection::open(data_dir.join("index.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS file_manifest(path TEXT PRIMARY KEY, mtime INTEGER NOT NULL);
         INSERT OR REPLACE INTO file_manifest(path, mtime) VALUES ('src/f0.rs', 123);",
    )
    .unwrap();
}

/// Seed a minimal `graph.json` whose nodes carry `names` — enough for
/// `reroute::grep_symbol::graph_resolves`'s raw byte scan (`"name":"..."`,
/// no whitespace) to resolve a symbol-shaped pattern before its deny fires.
fn seed_graph(data_dir: &Path, names: &[String]) {
    let nodes: Vec<Value> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            json!({"id": format!("n{i}"), "name": n, "kind": "function", "file": "src/f0.rs", "line": 1})
        })
        .collect();
    let graph = json!({"nodes": nodes, "edges": []});
    std::fs::write(data_dir.join("graph.json"), graph.to_string()).unwrap();
}

/// Run `lens hook claude <event>` with `payload` on stdin under a clean,
/// explicit full-steering env — mirrors `routing_tests.rs::run_hook`.
fn run_hook(event: &str, payload: &Value, data_dir: &Path) -> Value {
    let mut cmd = Command::new(bin());
    cmd.args(["hook", "claude", event])
        .env("LENS_DIR", data_dir)
        .env_remove("LENS_GREP_SCOPE_DENY")
        .env_remove("LENS_GREP_SYMBOL_DENY")
        .env_remove("LENS_READ_SKELETON_DENY")
        .env_remove("LENS_BASH_AGG_DENY")
        .env_remove("LENS_EDIT_LINKS_DENY")
        .env_remove("LENS_GREP_AST_DENY")
        .env_remove("LENS_READ_OVERVIEW_DENY")
        .env_remove("LENS_BASH_GREP_DENY")
        .env_remove("LENS_READ_RUNFILE_DENY")
        .env_remove("LENS_EDIT_LINKS_MIN_CALLERS")
        .env_remove("LENS_READ_OVERVIEW_THRESHOLD")
        .env("LENS_ROUTING", "full")
        .env("LENS_ROUTING_MCP", "up")
        // RTK coexistence (plan T4): deterministic regardless of whether RTK
        // happens to be installed + hooked on the host machine.
        .env("LENS_DEFER_BASH_TO_RTK", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn hook");
    {
        let mut si = child.stdin.take().unwrap();
        si.write_all(payload.to_string().as_bytes()).unwrap();
    }
    let out = child.wait_with_output().expect("hook output");
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    serde_json::from_str(&raw).unwrap_or(Value::Null)
}

/// The routing verdict grade a replayed call actually produced, in the same
/// vocabulary `decision_label()` (`src/routing/mod.rs`) uses: `deny` (PreToolUse
/// blocked it), `modify` (input rewritten, e.g. the `lens wrap --` prefix),
/// `passthrough` (untouched — includes the true no-op `{}` PreToolUse emits
/// when nothing fired).
fn grade_of(out: &Value) -> &'static str {
    let hso = &out["hookSpecificOutput"];
    match hso["permissionDecision"].as_str() {
        Some("deny") => "deny",
        Some("allow") if hso.get("updatedInput").is_some() => "modify",
        _ => "passthrough",
    }
}

fn replay(tool: &str, tool_input: &Value, prime_prompt: Option<&str>, graph_names: &[String]) -> &'static str {
    let d = tempfile::tempdir().unwrap();
    seed_index(d.path());
    seed_graph(d.path(), graph_names);
    let sess = "replay";
    if let Some(prompt) = prime_prompt {
        let p = json!({"session_id": sess, "cwd": d.path().to_string_lossy(), "prompt": prompt});
        run_hook("UserPromptSubmit", &p, d.path());
    }
    let payload = json!({
        "session_id": sess,
        "cwd": d.path().to_string_lossy(),
        "tool_name": tool,
        "tool_input": localize(tool_input),
    });
    let out = run_hook("PreToolUse", &payload, d.path());
    grade_of(&out)
}

#[derive(Deserialize)]
struct ReplayCase {
    tool: String,
    tool_input: Value,
    prime_prompt: Option<String>,
    #[serde(default)]
    graph_names: Vec<String>,
    expect: String,
}

fn load_jsonl<T: for<'de> Deserialize<'de>>(path: &str) -> Vec<T> {
    let full = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    let raw = std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {full:?}: {e}"));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("parse {l}: {e}")))
        .collect()
}

/// The routing.log-derived fixture: every event is independently replayable
/// (fresh tempdir/session per line — no cross-line state), so a single fresh
/// `RouteCtx`-equivalent env per case is all `Test structure` (plan) asks for.
#[test]
fn routing_log_replay_matches_post_plan_verdicts() {
    let cases: Vec<ReplayCase> = load_jsonl("tests/fixtures/routing_replay.jsonl");
    assert!(
        cases.len() >= 100,
        "routing.log-derived fixture must carry >= 100 events: {}",
        cases.len()
    );

    let mut bash_results = Vec::new();
    for (i, case) in cases.iter().enumerate() {
        let grade = replay(
            &case.tool,
            &case.tool_input,
            case.prime_prompt.as_deref(),
            &case.graph_names,
        );
        if case.tool == "Bash" {
            bash_results.push((i, case.tool_input["command"].as_str().unwrap_or("").to_string(), grade));
        }
        assert_eq!(
            grade, case.expect,
            "line {i} ({} {}): expected {}, got {grade}",
            case.tool, case.tool_input, case.expect
        );
    }

    // Predicate: all 7 original Bash events from the audit are non-passthrough
    // where they were broad greps (H1's new bash_grep arm now catches them).
    assert_eq!(bash_results.len(), 7, "fixture must carry all 7 real Bash events");
    let non_passthrough = bash_results.iter().filter(|(_, _, g)| *g != "passthrough").count();
    assert!(
        non_passthrough >= 5,
        "at least 5 of the 7 real Bash events were broad greps and must now be non-passthrough: {bash_results:?}"
    );
}

#[derive(Deserialize)]
struct MinedCase {
    tool: String,
    tool_input: Value,
    prime_prompt: Option<String>,
    #[serde(default)]
    graph_names: Vec<String>,
    quarantine: bool,
    expect: Option<String>,
    meta_pattern: Option<String>,
}

/// The mined-corpus fixture (98 real dogfooding cases). Non-quarantined cases
/// are verdict-asserted; quarantined ones (grep_ast DSL defect class, out of
/// scope per the plan) are only counted.
#[test]
fn mined_corpus_replay_matches_routing_verdicts() {
    let cases: Vec<MinedCase> = load_jsonl("tests/fixtures/mined_replay.jsonl");
    assert_eq!(cases.len(), 98, "mined corpus fixture must carry all 98 cases");

    let quarantined: Vec<&MinedCase> = cases.iter().filter(|c| c.quarantine).collect();
    assert_eq!(
        quarantined.len(),
        0,
        "quarantine count must be exactly 0 (all GA-FOLLOWUP cases de-quarantined): {:?}",
        quarantined.iter().map(|c| &c.meta_pattern).collect::<Vec<_>>()
    );

    let mut asserted = 0;
    for (i, case) in cases.iter().enumerate() {
        let expect = case
            .expect
            .as_deref()
            .unwrap_or_else(|| panic!("line {i}: case must carry an expect grade"));
        let grade = replay(
            &case.tool,
            &case.tool_input,
            case.prime_prompt.as_deref(),
            &case.graph_names,
        );
        assert_eq!(
            grade, expect,
            "line {i} ({} {}): expected {expect}, got {grade}",
            case.tool, case.tool_input
        );
        asserted += 1;
    }
    assert_eq!(asserted, 98, "every case must be verdict-asserted");
}
