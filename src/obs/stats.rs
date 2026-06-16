//! `ctxforge stats` — a human-readable view of the op log + store counters.
//!
//! Read-only and additive: it reconstructs cumulative savings from `ops.log`
//! (so totals reconcile with the log line-for-line) and reads store/graph sizes
//! from the counters the tools already maintain. Prints to its own stdout; the
//! MCP server's stdout is untouched.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::{data_dir, OpRecord};
use crate::session::store::SessionStore;
use crate::store::Store;

/// Window/scope filters parsed from the CLI flags.
#[derive(Default, Clone)]
struct Filters {
    session: Option<String>,
    since: Option<String>,
    last: Option<usize>,
}

/// CLI entry: `args` is everything after `stats`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let mut watch = false;
    let mut filters = Filters::default();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--watch" => watch = true,
            "--session" => {
                filters.session = args.get(i + 1).cloned();
                i += 1;
            }
            "--since" => {
                filters.since = args.get(i + 1).cloned();
                i += 1;
            }
            "--last" => {
                filters.last = args.get(i + 1).and_then(|v| v.parse().ok());
                i += 1;
            }
            other => {
                eprintln!("ctxforge stats: unknown flag '{other}'");
                eprintln!("usage: ctxforge stats [--watch] [--session <id>] [--since <iso8601>] [--last <n>]");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let dir = data_dir();
    if watch {
        // Refresh until interrupted. ~1s cadence; re-reads counters each tick.
        loop {
            print!("\x1b[2J\x1b[H"); // clear screen + home (cursor control, not color)
            let report = render(&dir, &filters);
            print!("{report}");
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs(1));
        }
    } else {
        print!("{}", render(&dir, &filters));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Aggregation
// ---------------------------------------------------------------------------

/// Per-tool rollup.
#[derive(Default)]
struct ToolAgg {
    count: u64,
    raw: u64,
    returned: u64,
    saved: i64,
    errors: u64,
    timeouts: u64,
}

/// Totals derived from the op log.
#[derive(Default)]
pub struct Totals {
    pub ops: u64,
    pub raw_bytes_in: u64,
    pub bytes_returned: u64,
    pub tokens_saved_est: i64,
    pub errors: u64,
    pub timeouts: u64,
    pub lock_wait_ms: u64,
    pub offloaded_ops: u64,
    pub offloaded_bytes: u64,
    by_tool: BTreeMap<String, ToolAgg>,
}

/// Aggregate a slice of records into [`Totals`]. Pure — the reconciliation test
/// drives this directly.
pub fn aggregate(records: &[OpRecord]) -> Totals {
    let mut t = Totals::default();
    for r in records {
        t.ops += 1;
        t.raw_bytes_in += r.raw_bytes_in;
        t.bytes_returned += r.bytes_returned;
        t.tokens_saved_est += r.tokens_saved_est;
        t.lock_wait_ms += r.lock_wait_ms;
        if r.outcome == "error" {
            t.errors += 1;
        }
        if r.outcome == "timed_out" {
            t.timeouts += 1;
        }
        if r.store_ref.is_some() && r.raw_bytes_in > r.bytes_returned {
            t.offloaded_ops += 1;
            t.offloaded_bytes += r.raw_bytes_in - r.bytes_returned;
        }
        let agg = t.by_tool.entry(r.tool.clone()).or_default();
        agg.count += 1;
        agg.raw += r.raw_bytes_in;
        agg.returned += r.bytes_returned;
        agg.saved += r.tokens_saved_est;
        if r.outcome == "error" {
            agg.errors += 1;
        }
        if r.outcome == "timed_out" {
            agg.timeouts += 1;
        }
    }
    t
}

/// Which mechanism a tool belongs to (sandbox / index / discovery / retrieve).
/// `rtk_shell` records (RTK's own shell-command savings, synced into the op log)
/// bucket under "shell" so they read as a distinct lane in the mechanism rollup.
fn mechanism(tool: &str) -> &'static str {
    match tool {
        "ctx_execute" => "sandbox",
        "ctx_index" | "ctx_search" => "index",
        "ctx_discover" | "graph_query" | "graph_neighbors" | "graph_path" => "discovery",
        "ctx_retrieve" => "retrieve",
        "rtk_shell" => "shell",
        _ => "other",
    }
}

/// Best-effort snapshot of RTK's *own* measured shell-command savings, read from
/// `rtk gain` (never re-estimated by ctxforge). Returns
/// `{"installed": false}` when RTK isn't installed or the call fails; otherwise
/// `{"installed": true, ...}` with RTK's global summary totals. Cheap: skips the
/// subprocess entirely unless a binary is resolvable.
fn rtk_snapshot() -> serde_json::Value {
    if !crate::rtk::rtk_available() {
        return serde_json::json!({ "installed": false });
    }
    match crate::rtk::gain::read_gain(crate::rtk::gain::Scope::Global) {
        Ok(g) => serde_json::json!({
            "installed": true,
            "total_commands": g.summary.total_commands,
            "total_saved": g.summary.total_saved,
            "avg_savings_pct": g.summary.avg_savings_pct,
            "total_input": g.summary.total_input,
            "total_output": g.summary.total_output,
        }),
        Err(_) => serde_json::json!({ "installed": false }),
    }
}

/// Read all op records under `dir` (rotated `ops.log.1` first, then `ops.log`,
/// preserving chronological order), then apply scope/window filters.
pub fn read_records(dir: &Path, filters_session: Option<&str>) -> Vec<OpRecord> {
    let mut out = Vec::new();
    let main = dir.join("ops.log");
    let rotated_log = rotated(&main);
    for path in [rotated_log, main] {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            for line in raw.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(rec) = serde_json::from_str::<OpRecord>(line) {
                    if let Some(s) = filters_session {
                        if rec.session_id.as_deref() != Some(s) {
                            continue;
                        }
                    }
                    out.push(rec);
                }
            }
        }
    }
    out
}

fn rotated(main: &Path) -> PathBuf {
    let mut s = main.to_path_buf().into_os_string();
    s.push(".1");
    PathBuf::from(s)
}

fn apply_window(mut records: Vec<OpRecord>, filters: &Filters) -> Vec<OpRecord> {
    if let Some(since) = &filters.since {
        records.retain(|r| r.ts.as_str() >= since.as_str());
    }
    if let Some(n) = filters.last {
        if records.len() > n {
            records = records.split_off(records.len() - n);
        }
    }
    records
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Store size + index/graph counts (the counters the tools maintain).
/// Best-effort: missing DB/counters read as zero.
fn store_index_graph(dir: &Path) -> (u64, i64, i64, i64) {
    let (index_chunks, graph_nodes, graph_edges) = match Store::open(dir) {
        Ok(s) => (
            s.get_stat("index_chunks").unwrap_or(0),
            s.get_stat("graph_nodes").unwrap_or(0),
            s.get_stat("graph_edges").unwrap_or(0),
        ),
        Err(_) => (0, 0, 0),
    };
    (db_size(dir, "store.db"), index_chunks, graph_nodes, graph_edges)
}

/// Aggregate `ops.log` (optionally scoped to a session) into a JSON snapshot —
/// the shape the web dashboard polls. Cumulative only; rate/throughput is the
/// client's job (it diffs successive snapshots), keeping this stateless.
pub fn snapshot_json(dir: &Path, session: Option<&str>) -> serde_json::Value {
    let records = read_records(dir, session);
    let t = aggregate(&records);
    let (store_size, index_chunks, graph_nodes, graph_edges) = store_index_graph(dir);

    let by_tool: Vec<serde_json::Value> = t
        .by_tool
        .iter()
        .map(|(tool, a)| {
            serde_json::json!({
                "tool": tool, "ops": a.count, "raw": a.raw,
                "returned": a.returned, "saved": a.saved,
                "errors": a.errors, "timeouts": a.timeouts,
            })
        })
        .collect();

    let mut mech: BTreeMap<&str, (u64, i64)> = BTreeMap::new();
    for (tool, a) in &t.by_tool {
        let e = mech.entry(mechanism(tool)).or_default();
        e.0 += a.count;
        e.1 += a.saved;
    }
    let by_mechanism: Vec<serde_json::Value> = mech
        .iter()
        .map(|(m, (ops, saved))| serde_json::json!({ "mechanism": m, "ops": ops, "saved": saved }))
        .collect();

    // The MCP-tool-savings plane must read MCP-only: exclude the synced `rtk_shell`
    // op so a `ctxforge rtk sync` doesn't relabel RTK's shell savings as MCP savings.
    // RTK savings keep their own plane (`rtk` block + `by_mechanism` "shell"); the
    // grand-total `tokens_saved_est` still includes them (the ledger reconciles).
    let rtk_shell_saved: i64 = t.by_tool.get("rtk_shell").map(|a| a.saved).unwrap_or(0);
    let tokens_saved_mcp = t.tokens_saved_est - rtk_shell_saved;

    // The "first plane": built-in tool activity captured by the session hooks.
    let activity = SessionStore::open(dir)
        .and_then(|s| s.activity(session))
        .unwrap_or_default();
    let act_categories: Vec<serde_json::Value> = activity
        .by_category
        .iter()
        .map(|(c, n)| serde_json::json!({ "category": c, "count": n }))
        .collect();
    let act_hooks: Vec<serde_json::Value> = activity
        .by_hook
        .iter()
        .map(|(h, n)| serde_json::json!({ "hook": h, "count": n }))
        .collect();

    serde_json::json!({
        "ts": super::iso8601_now(),
        "ops": t.ops,
        "raw_bytes_in": t.raw_bytes_in,
        "bytes_returned": t.bytes_returned,
        "tokens_saved_est": t.tokens_saved_est,
        "tokens_saved_mcp": tokens_saved_mcp,
        "errors": t.errors,
        "timeouts": t.timeouts,
        "lock_wait_ms": t.lock_wait_ms,
        "offloaded_ops": t.offloaded_ops,
        "offloaded_bytes": t.offloaded_bytes,
        "by_tool": by_tool,
        "by_mechanism": by_mechanism,
        "rtk": rtk_snapshot(),
        "store_size": store_size,
        "index_chunks": index_chunks,
        "graph_nodes": graph_nodes,
        "graph_edges": graph_edges,
        "session": session,
        "activity": {
            "total_events": activity.total_events,
            "sessions": activity.sessions,
            "last_ts": activity.last_ts,
            "by_category": act_categories,
            "by_hook": act_hooks,
        },
    })
}

fn render(dir: &Path, filters: &Filters) -> String {
    let records = apply_window(read_records(dir, filters.session.as_deref()), filters);
    let t = aggregate(&records);

    // Store/graph sizes from the counters the tools maintain (best-effort).
    let (store_size, index_chunks, graph_nodes, graph_edges) = store_index_graph(dir);

    let mut o = String::new();
    o.push_str("ctxforge stats\n");
    o.push_str(&format!("  data dir        : {}\n", dir.display()));
    if let Some(s) = &filters.session {
        o.push_str(&format!("  session         : {s}\n"));
    }
    if let Some(s) = &filters.since {
        o.push_str(&format!("  since           : {s}\n"));
    }
    if let Some(n) = filters.last {
        o.push_str(&format!("  window          : last {n} ops\n"));
    }
    o.push('\n');

    o.push_str(&format!("  ops total       : {}\n", t.ops));
    o.push_str(&format!("  raw bytes in    : {}\n", human_bytes(t.raw_bytes_in)));
    o.push_str(&format!("  bytes returned  : {}\n", human_bytes(t.bytes_returned)));
    o.push_str(&format!(
        "  tokens saved est: {} (~{})\n",
        t.tokens_saved_est,
        human_count(t.tokens_saved_est.max(0) as u64)
    ));
    o.push_str(&format!(
        "  offloaded       : {} ops, {} moved to store\n",
        t.offloaded_ops,
        human_bytes(t.offloaded_bytes)
    ));
    o.push_str(&format!("  errors / timeouts: {} / {}\n", t.errors, t.timeouts));
    o.push_str(&format!("  lock wait total : {} ms\n", t.lock_wait_ms));
    o.push('\n');

    // By tool.
    o.push_str("  by tool:\n");
    o.push_str(&format!(
        "    {:<16} {:>6} {:>11} {:>11} {:>11}\n",
        "tool", "ops", "raw", "returned", "saved~tok"
    ));
    for (tool, a) in &t.by_tool {
        o.push_str(&format!(
            "    {:<16} {:>6} {:>11} {:>11} {:>11}\n",
            tool,
            a.count,
            human_bytes(a.raw),
            human_bytes(a.returned),
            a.saved,
        ));
    }
    o.push('\n');

    // By mechanism.
    let mut mech: BTreeMap<&str, (u64, i64)> = BTreeMap::new();
    for (tool, a) in &t.by_tool {
        let e = mech.entry(mechanism(tool)).or_default();
        e.0 += a.count;
        e.1 += a.saved;
    }
    o.push_str("  by mechanism:\n");
    for (m, (ops, saved)) in &mech {
        o.push_str(&format!("    {m:<16} {ops:>6} ops  {saved:>11} saved~tok\n"));
    }
    o.push('\n');

    // RTK shell savings: RTK's *own* measured savings on shell commands (via
    // `rtk gain`) — never re-estimated by ctxforge. Best-effort; degrades to a
    // "not installed" line when no RTK binary resolves.
    let rtk = rtk_snapshot();
    o.push_str("  RTK shell savings (rtk gain):\n");
    if rtk.get("installed").and_then(|v| v.as_bool()) == Some(true) {
        let cmds = rtk.get("total_commands").and_then(|v| v.as_u64()).unwrap_or(0);
        let saved = rtk.get("total_saved").and_then(|v| v.as_u64()).unwrap_or(0);
        let pct = rtk.get("avg_savings_pct").and_then(|v| v.as_f64()).unwrap_or(0.0);
        o.push_str(&format!("    commands        : {cmds}\n"));
        o.push_str(&format!(
            "    tokens saved    : {saved} (~{})\n",
            human_count(saved)
        ));
        o.push_str(&format!("    avg savings     : {pct:.1}%\n"));
    } else {
        o.push_str("    not installed   : run `ctxforge rtk install` to enable\n");
    }
    o.push('\n');

    // Store / index / graph sizes.
    o.push_str("  store / index / graph:\n");
    o.push_str(&format!("    store size      : {}\n", human_bytes(store_size)));
    o.push_str(&format!("    index chunks    : {index_chunks}\n"));
    o.push_str(&format!("    graph nodes     : {graph_nodes}\n"));
    o.push_str(&format!("    graph edges     : {graph_edges}\n"));

    // Session activity: the "first plane" — built-in tool use captured by the
    // hooks (NOT ctxforge MCP tool usage; that is the savings totals above).
    let activity = SessionStore::open(dir)
        .and_then(|s| s.activity(filters.session.as_deref()))
        .unwrap_or_default();
    o.push('\n');
    o.push_str("  session activity (built-in tools, via hooks):\n");
    o.push_str(&format!(
        "    events total    : {} (across {} session(s))\n",
        activity.total_events, activity.sessions
    ));
    for (cat, n) in &activity.by_category {
        o.push_str(&format!("    {cat:<16}: {n}\n"));
    }
    o
}

fn db_size(dir: &Path, name: &str) -> u64 {
    let base = dir.join(name);
    let mut total = std::fs::metadata(&base).map(|m| m.len()).unwrap_or(0);
    for suffix in ["-wal", "-shm"] {
        let mut p = base.clone().into_os_string();
        p.push(suffix);
        if let Ok(m) = std::fs::metadata(PathBuf::from(p)) {
            total += m.len();
        }
    }
    total
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} {}", UNITS[0])
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

fn human_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::OpLog;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn totals_reconcile_with_log() {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        log.start("ctx_execute", json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
        log.start("ctx_search", json!({}))
            .finish(50, 50, None, "ok", "", None);
        log.start("ctx_execute", json!({}))
            .finish(0, 0, None, "error", "boom", None);

        let records = read_records(dir.path(), None);
        let t = aggregate(&records);
        assert_eq!(t.ops, 3);
        assert_eq!(t.raw_bytes_in, 8050);
        assert_eq!(t.bytes_returned, 150);
        assert_eq!(t.tokens_saved_est, (8000 - 100) / 4);
        assert_eq!(t.errors, 1);
        assert_eq!(t.offloaded_ops, 1);
        assert_eq!(t.offloaded_bytes, 7900);
        assert_eq!(t.by_tool["ctx_execute"].count, 2);
        // Sum of per-record returned bytes matches the aggregate exactly.
        let summed: u64 = records.iter().map(|r| r.bytes_returned).sum();
        assert_eq!(summed, t.bytes_returned);
    }

    #[test]
    fn session_filter_scopes_records() {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        std::env::set_var("CTXFORGE_SESSION_ID", "s1");
        log.start("ctx_search", json!({})).finish(1, 1, None, "ok", "", None);
        std::env::set_var("CTXFORGE_SESSION_ID", "s2");
        log.start("ctx_search", json!({})).finish(1, 1, None, "ok", "", None);
        std::env::remove_var("CTXFORGE_SESSION_ID");

        let only_s1 = read_records(dir.path(), Some("s1"));
        assert_eq!(only_s1.len(), 1);
        assert_eq!(only_s1[0].session_id.as_deref(), Some("s1"));
    }

    /// Write an executable stub `<home>/bin/rtk` that answers `--version` and
    /// `gain --format json` (tolerating a trailing `--project`), so RTK-dependent
    /// surfacing can be tested offline against canned totals (RTK_NOTES.md §8).
    #[cfg(unix)]
    fn write_stub_rtk(home: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let script = bin.join("rtk");
        // total_saved=2268362, total_commands=3753, avg_savings_pct=61.47..., etc.
        std::fs::write(
            &script,
            r#"#!/bin/sh
case "$1" in
  --version) echo "rtk 0.28.2" ;;
  gain) cat <<'JSON'
{"summary":{"total_commands":3753,"total_input":3689788,"total_output":1424127,"total_saved":2268362,"avg_savings_pct":61.47675693020845,"total_time_ms":2990161,"avg_time_ms":796}}
JSON
  ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// RTK-dependent assertions live in ONE test: `CTXFORGE_HOME` is process-global
    /// and `cargo test` runs in parallel, so we set it, run every rtk assertion,
    /// then remove it within this single test to keep the mutation tightly scoped.
    /// Data dirs are independent tempdirs passed straight to `snapshot_json`.
    #[cfg(unix)]
    #[test]
    fn rtk_plane_present_and_absent_and_buckets_shell() {
        // Serialize with other CTXFORGE_HOME mutators (env is process-global).
        let _g = crate::rtk::env_test_lock();
        // --- 1. PRESENT: point CTXFORGE_HOME at a home holding the stub rtk. ---
        let home = tempdir().unwrap();
        write_stub_rtk(home.path());
        std::env::set_var("CTXFORGE_HOME", home.path());

        // Seed an `rtk_shell` OpRecord (RTK's own number, raw/returned bytes 0)
        // plus a regular MCP op, so by_mechanism/by_tool can be checked.
        let data = tempdir().unwrap();
        let log = OpLog::open(data.path());
        log.start("ctx_execute", json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
        // OpHandle::finish derives tokens_saved_est from bytes; rtk_shell carries
        // RTK's own figure instead, so append the record directly.
        log.append(&OpRecord {
            ts: super::super::iso8601_now(),
            session_id: None,
            agent_id: "test".into(),
            pid: std::process::id(),
            tool: "rtk_shell".into(),
            input_summary: json!({"total_input": 100, "total_output": 40}),
            raw_bytes_in: 0,
            bytes_returned: 0,
            tokens_saved_est: 60,
            store_ref: None,
            duration_ms: 0,
            lock_wait_ms: 0,
            outcome: "ok".into(),
            note: String::new(),
        });

        let snap = snapshot_json(data.path(), None);
        let rtk = &snap["rtk"];
        assert_eq!(rtk["installed"], json!(true));
        assert_eq!(rtk["total_commands"], json!(3753));
        assert_eq!(rtk["total_saved"], json!(2268362));
        assert_eq!(rtk["total_input"], json!(3689788));
        assert_eq!(rtk["total_output"], json!(1424127));
        assert!((rtk["avg_savings_pct"].as_f64().unwrap() - 61.476_756_930_208_45).abs() < 1e-9);

        // by_mechanism has a "shell" lane carrying the rtk_shell op's saved tokens.
        let mechs = snap["by_mechanism"].as_array().unwrap();
        let shell = mechs
            .iter()
            .find(|m| m["mechanism"] == json!("shell"))
            .expect("by_mechanism should include a 'shell' lane for rtk_shell");
        assert_eq!(shell["ops"], json!(1));
        assert_eq!(shell["saved"], json!(60));
        // by_tool includes the rtk_shell tool automatically.
        let tools = snap["by_tool"].as_array().unwrap();
        assert!(
            tools.iter().any(|t| t["tool"] == json!("rtk_shell")),
            "by_tool should include rtk_shell, got: {tools:?}"
        );

        // --- 2. ABSENT: empty home (no bin/rtk). PATH has no managed rtk here
        // (the real binary lives only under ~/.ctxforge, which we've overridden).
        let empty = tempdir().unwrap();
        std::env::set_var("CTXFORGE_HOME", empty.path());
        let snap2 = snapshot_json(data.path(), None);
        let installed = snap2["rtk"]["installed"].as_bool();
        // Normally false; if a stray `rtk` is on PATH it could read true — either
        // way "installed" must be a bool and the key must exist.
        assert!(installed.is_some(), "rtk.installed must be a bool");
        if installed == Some(false) {
            assert_eq!(snap2["rtk"], json!({ "installed": false }));
        }

        std::env::remove_var("CTXFORGE_HOME");
    }
}
