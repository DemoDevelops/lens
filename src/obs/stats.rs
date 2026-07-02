//! `lens stats` — a human-readable view of the op log + store counters.
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
                eprintln!("lens stats: unknown flag '{other}'");
                eprintln!(
                    "usage: lens stats [--watch] [--session <id>] [--since <iso8601>] [--last <n>]"
                );
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
    /// Ops that actually offloaded large output to the store (the only *measured*
    /// savings — distinct from adoption, which is just `count`).
    offloaded_ops: u64,
    offloaded_bytes: u64,
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
        if r.store_ref.is_some() && r.raw_bytes_in > r.bytes_returned {
            agg.offloaded_ops += 1;
            agg.offloaded_bytes += r.raw_bytes_in - r.bytes_returned;
        }
    }
    t
}

/// Which mechanism a tool belongs to (darkroom / index / discovery / retrieve).
/// `rtk_shell` records (RTK's own shell-command savings, synced into the op log)
/// bucket under "shell" so they read as a distinct lane in the mechanism rollup.
fn mechanism(tool: &str) -> &'static str {
    match tool {
        "lens_run" => "darkroom",
        "lens_index" | "lens_search" => "index",
        "lens_map" | "lens_symbol" | "lens_links" | "lens_path" | "lens_find" => "discovery",
        "lens_recall" => "retrieve",
        "rtk_shell" => "shell",
        _ => "other",
    }
}

/// Best-effort snapshot of RTK's *own* measured shell-command savings, read from
/// `rtk gain` (never re-estimated by lens). Returns
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
///
/// Streams each log through a buffered reader line-by-line rather than slurping
/// the whole file into memory first, so a multi-megabyte log never materializes
/// as one allocation. Records and order are identical to the prior read-all path.
pub fn read_records(dir: &Path, filters_session: Option<&str>) -> Vec<OpRecord> {
    use std::io::{BufRead, BufReader};
    let mut out = Vec::new();
    let main = dir.join("ops.log");
    let rotated_log = rotated(&main);
    for path in [rotated_log, main] {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(rec) = serde_json::from_str::<OpRecord>(&line) {
                if let Some(s) = filters_session {
                    if rec.session_id.as_deref() != Some(s) {
                        continue;
                    }
                }
                out.push(rec);
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
    (
        db_size(dir, "store.db"),
        index_chunks,
        graph_nodes,
        graph_edges,
    )
}

/// The top-level snapshot keys that name a display panel/dimension. Both renderers
/// (web `INDEX_HTML`, terminal `tui::render_snapshot`) must surface every one; the
/// parity tripwire test in `dashboard.rs` scans both for each key. Adding a panel =
/// add its key here, then both renderers (or the tripwire fails).
pub const SNAPSHOT_DIMENSIONS: &[&str] = &[
    "tokens_saved_mcp",
    "by_tool",
    "by_mechanism",
    "applied_value",
    "rtk",
    "activity",
    "store_size",
];

/// Aggregate `ops.log` (optionally scoped to a session) into a JSON snapshot —
/// the shape the web dashboard polls. Cumulative; rate/throughput is the client's
/// job (it diffs successive snapshots), keeping this stateless.
pub fn snapshot_json(dir: &Path, session: Option<&str>) -> serde_json::Value {
    snapshot_json_since(dir, session, None, None)
}

/// As [`snapshot_json`], but `since` (unix seconds) restricts ops + session
/// activity to events at/after that instant — the dashboard's "live since the page
/// loaded" scope, so only currently-active sessions show, never previous ones. The
/// `rtk` plane stays global (it reflects `rtk gain`, not this data dir's sessions).
pub fn snapshot_json_since(
    dir: &Path,
    session: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
) -> serde_json::Value {
    let mut records = read_records(dir, session);
    if let Some(cut) = since {
        let iso = super::iso8601_secs(cut);
        records.retain(|r| r.ts.as_str() >= iso.as_str());
    }
    // Upper bound for an explicit range (half-open, matching the lower bound); open to
    // now when unset. `until <= 0` means "no upper bound" (the sentinel the dashboard sends).
    if let Some(cut) = until.filter(|&u| u > 0) {
        let iso = super::iso8601_secs(cut);
        records.retain(|r| r.ts.as_str() < iso.as_str());
    }
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
                "offloaded_ops": a.offloaded_ops, "offloaded_bytes": a.offloaded_bytes,
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
    // op so a `lens rtk sync` doesn't relabel RTK's shell savings as MCP savings.
    // RTK savings keep their own plane (`rtk` block + `by_mechanism` "shell"); the
    // grand-total `tokens_saved_est` still includes them (the ledger reconciles).
    let rtk_shell_saved: i64 = t.by_tool.get("rtk_shell").map(|a| a.saved).unwrap_or(0);
    let tokens_saved_mcp = t.tokens_saved_est - rtk_shell_saved;

    // Applied-value plane: benchmark per-op rates × this scope's live op counts, so the
    // dashboard accumulates the counterfactual value (tokens, round-trips) the
    // byte-delta ledger scores as zero. Estimates only (`estimate: true`); they never
    // enter `tokens_saved_est`/`tokens_saved_mcp` or the `$` headline. Rates are
    // drift-guarded against the committed benchmark JSONs in value_model.
    let applied_value = super::value_model::applied_value_json(
        |tool| t.by_tool.get(tool).map(|a| a.count).unwrap_or(0),
        tokens_saved_mcp,
    );

    // The "first plane": built-in tool activity captured by the session hooks.
    let activity = SessionStore::open(dir)
        .and_then(|s| s.activity_range(session, since, until))
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

    // Bucketed timelines for the dashboard's windowed sparklines + cumulative
    // chart. Computed server-side so the curves reflect the whole selected
    // window, not just samples taken since the page opened. `since == 0`
    // ("all time") falls back to the earliest op so the axis starts at first
    // activity, not 1970. Cumulative; the client diffs for per-bucket rates.
    const BUCKETS: usize = 60;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let start = since.filter(|&s| s > 0).unwrap_or_else(|| {
        records
            .iter()
            .filter_map(|r| super::iso8601_to_secs(&r.ts))
            .min()
            .unwrap_or(now)
    });
    // Window end: the explicit `until` for a range, else now. Buckets + window_end
    // span [start, end] so a bounded range renders its own timeline, not up to now.
    let end = until.filter(|&u| u > 0).unwrap_or(now);
    let span = (end - start).max(1);
    let bucket = |ts: i64| {
        (((ts - start) as f64 / span as f64).clamp(0.0, 1.0) * BUCKETS as f64) as usize
    };
    let cumulative = |b: &[i64]| -> Vec<i64> {
        let mut acc = 0i64;
        b.iter()
            .map(|x| {
                acc += x;
                acc
            })
            .collect()
    };
    let (mut saved_b, mut bytes_b) = (vec![0i64; BUCKETS], vec![0i64; BUCKETS]);
    for r in &records {
        if let Some(ts) = super::iso8601_to_secs(&r.ts) {
            let i = bucket(ts).min(BUCKETS - 1);
            bytes_b[i] += r.bytes_returned as i64;
            // saved is MCP-only (excludes the synced rtk_shell op), matching tokens_saved_mcp.
            if r.tool != "rtk_shell" {
                saved_b[i] += r.tokens_saved_est;
            }
        }
    }
    let saved_buckets = cumulative(&saved_b);
    let bytes_buckets = cumulative(&bytes_b);
    let event_buckets = cumulative(
        &SessionStore::open(dir)
            .and_then(|s| s.event_buckets(session, start, end, BUCKETS))
            .unwrap_or_else(|_| vec![0i64; BUCKETS]),
    );

    serde_json::json!({
        "ts": super::iso8601_now(),
        "ops": t.ops,
        "raw_bytes_in": t.raw_bytes_in,
        "bytes_returned": t.bytes_returned,
        "tokens_saved_est": t.tokens_saved_est,
        "tokens_saved_mcp": tokens_saved_mcp,
        "saved_buckets": saved_buckets,
        "bytes_buckets": bytes_buckets,
        "event_buckets": event_buckets,
        "window_start": start,
        "window_end": end,
        "errors": t.errors,
        "timeouts": t.timeouts,
        "lock_wait_ms": t.lock_wait_ms,
        "offloaded_ops": t.offloaded_ops,
        "offloaded_bytes": t.offloaded_bytes,
        "by_tool": by_tool,
        "by_mechanism": by_mechanism,
        "applied_value": applied_value,
        "rtk": rtk_snapshot(),
        "store_size": store_size,
        "index_chunks": index_chunks,
        "graph_nodes": graph_nodes,
        "graph_edges": graph_edges,
        "session": session,
        "since": since,
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
    o.push_str("lens stats\n");
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
    o.push_str(&format!(
        "  raw bytes in    : {}\n",
        human_bytes(t.raw_bytes_in)
    ));
    o.push_str(&format!(
        "  bytes returned  : {}\n",
        human_bytes(t.bytes_returned)
    ));
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
    o.push_str(&format!(
        "  errors / timeouts: {} / {}\n",
        t.errors, t.timeouts
    ));
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
        o.push_str(&format!(
            "    {m:<16} {ops:>6} ops  {saved:>11} saved~tok\n"
        ));
    }
    o.push('\n');

    // RTK shell savings: RTK's *own* measured savings on shell commands (via
    // `rtk gain`) — never re-estimated by lens. Best-effort; degrades to a
    // "not installed" line when no RTK binary resolves.
    let rtk = rtk_snapshot();
    o.push_str("  RTK shell savings (rtk gain):\n");
    if rtk.get("installed").and_then(|v| v.as_bool()) == Some(true) {
        let cmds = rtk
            .get("total_commands")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let saved = rtk.get("total_saved").and_then(|v| v.as_u64()).unwrap_or(0);
        let pct = rtk
            .get("avg_savings_pct")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        o.push_str(&format!("    commands        : {cmds}\n"));
        o.push_str(&format!(
            "    tokens saved    : {saved} (~{})\n",
            human_count(saved)
        ));
        o.push_str(&format!("    avg savings     : {pct:.1}%\n"));
    } else {
        o.push_str("    not installed   : run `lens rtk install` to enable\n");
    }
    o.push('\n');

    // Store / index / graph sizes.
    o.push_str("  store / index / graph:\n");
    o.push_str(&format!(
        "    store size      : {}\n",
        human_bytes(store_size)
    ));
    o.push_str(&format!("    index chunks    : {index_chunks}\n"));
    o.push_str(&format!("    graph nodes     : {graph_nodes}\n"));
    o.push_str(&format!("    graph edges     : {graph_edges}\n"));

    // Session activity: the "first plane" — built-in tool use captured by the
    // hooks (NOT lens MCP tool usage; that is the savings totals above).
    let activity = SessionStore::open(dir)
        .and_then(|s| s.activity(filters.session.as_deref(), None))
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

pub(crate) fn human_bytes(n: u64) -> String {
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

pub(crate) fn human_count(n: u64) -> String {
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
        log.start("lens_run", json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
        log.start("lens_search", json!({}))
            .finish(50, 50, None, "ok", "", None);
        log.start("lens_run", json!({}))
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
        assert_eq!(t.by_tool["lens_run"].count, 2);
        // Sum of per-record returned bytes matches the aggregate exactly.
        let summed: u64 = records.iter().map(|r| r.bytes_returned).sum();
        assert_eq!(summed, t.bytes_returned);
    }

    #[test]
    fn session_filter_scopes_records() {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        std::env::set_var("LENS_SESSION_ID", "s1");
        log.start("lens_search", json!({}))
            .finish(1, 1, None, "ok", "", None);
        std::env::set_var("LENS_SESSION_ID", "s2");
        log.start("lens_search", json!({}))
            .finish(1, 1, None, "ok", "", None);
        std::env::remove_var("LENS_SESSION_ID");

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

    /// RTK-dependent assertions live in ONE test: `LENS_HOME` is process-global
    /// and `cargo test` runs in parallel, so we set it, run every rtk assertion,
    /// then remove it within this single test to keep the mutation tightly scoped.
    /// Data dirs are independent tempdirs passed straight to `snapshot_json`.
    #[cfg(unix)]
    #[test]
    fn rtk_plane_present_and_absent_and_buckets_shell() {
        // Serialize with other LENS_HOME mutators (env is process-global).
        let _g = crate::rtk::env_test_lock();
        // --- 1. PRESENT: point LENS_HOME at a home holding the stub rtk. ---
        let home = tempdir().unwrap();
        write_stub_rtk(home.path());
        std::env::set_var("LENS_HOME", home.path());

        // Seed an `rtk_shell` OpRecord (RTK's own number, raw/returned bytes 0)
        // plus a regular MCP op, so by_mechanism/by_tool can be checked.
        let data = tempdir().unwrap();
        let log = OpLog::open(data.path());
        log.start("lens_run", json!({}))
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
        // (the real binary lives only under ~/.lens, which we've overridden).
        let empty = tempdir().unwrap();
        std::env::set_var("LENS_HOME", empty.path());
        let snap2 = snapshot_json(data.path(), None);
        let installed = snap2["rtk"]["installed"].as_bool();
        // Normally false; if a stray `rtk` is on PATH it could read true — either
        // way "installed" must be a bool and the key must exist.
        assert!(installed.is_some(), "rtk.installed must be a bool");
        if installed == Some(false) {
            assert_eq!(snap2["rtk"], json!({ "installed": false }));
        }

        std::env::remove_var("LENS_HOME");
    }

    #[test]
    fn snapshot_since_scopes_ops_to_the_cutoff() {
        let dir = tempdir().unwrap();
        OpLog::open(dir.path()).start("lens_run", json!({})).finish(
            8000,
            100,
            Some("a".into()),
            "ok",
            "",
            None,
        );

        // No cutoff: the op is counted (today's behavior, unchanged).
        assert_eq!(snapshot_json(dir.path(), None)["ops"], json!(1));

        // Cutoff in the far future ⇒ the op (recorded "now") is before it ⇒ excluded.
        let future = 32_503_680_000; // ~year 3000
        let scoped = snapshot_json_since(dir.path(), None, Some(future), None);
        assert_eq!(scoped["ops"], json!(0), "ops before the cutoff are dropped");
        assert_eq!(scoped["tokens_saved_est"], json!(0));
        assert_eq!(scoped["since"], json!(future));
        assert!(scoped["by_tool"].as_array().unwrap().is_empty());

        // Cutoff at the epoch ⇒ everything is at/after it ⇒ included.
        assert_eq!(
            snapshot_json_since(dir.path(), None, Some(0), None)["ops"],
            json!(1)
        );
    }

    #[test]
    fn snapshot_exposes_applied_value() {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        // Seed one op each across measured + counterfactual dimensions.
        log.start("lens_run", json!({}))
            .finish(8000, 100, Some("a".into()), "ok", "", None);
        log.start("lens_search", json!({}))
            .finish(50, 50, None, "ok", "", None);
        log.start("lens_symbol", json!({}))
            .finish(40, 40, None, "ok", "", None);
        log.start("lens_recall", json!({}))
            .finish(20, 20, None, "ok", "", None);

        let snap = snapshot_json(dir.path(), None);
        let av = &snap["applied_value"];
        assert_eq!(av["model"], json!(crate::obs::value_model::VALUE_MODEL_MODEL));
        assert_eq!(av["note"], json!(crate::obs::value_model::VALUE_MODEL_NOTE));
        assert_eq!(av["estimate"], json!(true));
        // The measured plane is the session's real savings; estimates sit beside it.
        assert_eq!(av["measured_tokens"], snap["tokens_saved_mcp"]);
        let est = av["est_counterfactual_tokens"].as_i64().unwrap();
        assert!(est > 0, "search + recall contribute counterfactual tokens");
        assert_eq!(
            av["est_total_tokens"].as_i64().unwrap(),
            av["measured_tokens"].as_i64().unwrap() + est
        );
        assert!(av["round_trips_avoided"].as_f64().unwrap() > 0.0);

        let rows = av["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 5, "one row per value dimension");
        let nav = rows.iter().find(|r| r["dimension"] == json!("navigation")).unwrap();
        assert_eq!(nav["ops"], json!(1));
        assert!(nav["round_trips"].as_f64().unwrap() > 1.0, "nav avoids >1 round-trip/op");
        let rec = rows.iter().find(|r| r["dimension"] == json!("recovery")).unwrap();
        assert_eq!(rec["ops"], json!(1));
        assert!(rec["est_tokens"].as_i64().unwrap() > 0, "recall saves counterfactual tokens");

        // The applied-value block must NOT perturb the measured savings ledger.
        let t = aggregate(&read_records(dir.path(), None));
        assert_eq!(snap["tokens_saved_est"], json!(t.tokens_saved_est));
        assert_eq!(snap["tokens_saved_mcp"], json!(t.tokens_saved_est)); // no rtk_shell op here
    }

    #[test]
    fn applied_value_rows_exist_with_zero_ops() {
        let dir = tempdir().unwrap();
        let snap = snapshot_json(dir.path(), None);
        let av = &snap["applied_value"];
        assert_eq!(av["rows"].as_array().unwrap().len(), 5, "rows exist with no live ops");
        assert_eq!(av["round_trips_avoided"], json!(0.0));
        assert_eq!(av["est_total_tokens"], json!(0));
        // Every SNAPSHOT_DIMENSIONS key is actually present in a real snapshot.
        for key in SNAPSHOT_DIMENSIONS {
            assert!(snap.get(*key).is_some(), "snapshot missing dimension key '{key}'");
        }
    }
}
