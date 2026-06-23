//! Observability: a side channel that makes a live lens run watchable.
//!
//! Three pieces, all **additive** — no tool result payload changes, and nothing
//! here ever writes to the MCP server's stdout (that is JSON-RPC only):
//!   * an always-on, append-only operation log (`.lens/ops.log`, JSONL) —
//!     one summary record per tool invocation (see [`OpRecord`]);
//!   * an opt-in per-op decision trace (`.lens/explain.log`, `LENS_EXPLAIN=1`);
//!   * concurrency plumbing shared by every SQLite store ([`configure_conn`]):
//!     WAL mode + a busy handler that both retries and accounts for time spent
//!     waiting on a locked DB, surfaced as `lock_wait_ms`.
//!
//! The `lens stats` / `lens verify` CLI subcommands ([`stats`], [`verify`])
//! read these files back; they are separate processes whose stdout is their own.

pub mod dashboard;
pub mod stats;
pub mod verify;

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default ops.log size cap before rotation (~10 MB).
const DEFAULT_OPS_LOG_MAX: u64 = 10 * 1024 * 1024;

/// Process-global accumulator of milliseconds spent waiting on a busy SQLite DB
/// (see [`busy_handler`]). Monotonic for the life of the process; op records
/// capture the delta across a single operation.
pub static LOCK_WAIT_MS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Operation record (one JSONL line in ops.log)
// ---------------------------------------------------------------------------

/// One structured record per tool invocation. Summaries only — never full
/// inputs/outputs (those live in the reversible store, recoverable via
/// `store_ref`). Field order/names follow the observability plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpRecord {
    pub ts: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub session_id: Option<String>,
    pub agent_id: String,
    pub pid: u32,
    pub tool: String,
    pub input_summary: Value,
    pub raw_bytes_in: u64,
    pub bytes_returned: u64,
    pub tokens_saved_est: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub store_ref: Option<String>,
    pub duration_ms: u64,
    /// Time spent waiting on a locked DB during this op; omitted when zero.
    #[serde(skip_serializing_if = "is_zero", default)]
    pub lock_wait_ms: u64,
    pub outcome: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub note: String,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

// ---------------------------------------------------------------------------
// Operation log
// ---------------------------------------------------------------------------

/// Append-only writer for `ops.log` (and, when enabled, `explain.log`). Cheap to
/// clone (just paths + flags); safe to use from concurrent tasks because each
/// record is written as a single appended line (see [`OpLog::append`]).
#[derive(Clone)]
pub struct OpLog {
    path: PathBuf,
    explain_path: PathBuf,
    max_bytes: u64,
    explain_enabled: bool,
    /// Machine-global mirror of `ops.log` under `home_root()`, so the dashboard can
    /// total savings across every repo and launch profile. `None` when this data dir
    /// already is the global home (no self-mirror).
    global_path: Option<PathBuf>,
}

impl OpLog {
    /// Open the op log under `dir`. Honors `LENS_OPS_LOG_MAX` (rotation cap)
    /// and `LENS_EXPLAIN` (verbose trace). Never fails: logging must not be
    /// able to break a tool call.
    pub fn open(dir: &Path) -> Self {
        let _ = std::fs::create_dir_all(dir);
        let max_bytes = std::env::var("LENS_OPS_LOG_MAX")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(DEFAULT_OPS_LOG_MAX);
        let path = dir.join("ops.log");
        let global_path = global_ops_path(&path);
        if let Some(g) = &global_path {
            if let Some(parent) = g.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
        }
        OpLog {
            path,
            explain_path: dir.join("explain.log"),
            max_bytes,
            explain_enabled: explain_env(),
            global_path,
        }
    }

    /// True if `LENS_EXPLAIN` requested the verbose per-op trace.
    pub fn explain_enabled(&self) -> bool {
        self.explain_enabled
    }

    /// The active session id the hook published to `<data_dir>/current_session`, if
    /// any. This is how the long-lived MCP server learns which Claude session its
    /// tool calls belong to (it never sees the per-event hook payload). `None` when
    /// no hook has run yet or the file is empty/unreadable.
    fn current_session(&self) -> Option<String> {
        let dir = self.path.parent()?;
        let s = std::fs::read_to_string(dir.join("current_session")).ok()?;
        let s = s.trim();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    }

    /// Build the explain trail only when enabled, so callers pay nothing when off.
    pub fn explain<F: FnOnce() -> String>(&self, f: F) -> Option<String> {
        if self.explain_enabled {
            Some(f())
        } else {
            None
        }
    }

    /// Begin timing an operation. Captures the start instant and the current
    /// lock-wait baseline so the finished record reflects this op alone.
    pub fn start(&self, tool: &'static str, input_summary: Value) -> OpHandle {
        OpHandle {
            log: self.clone(),
            tool,
            input_summary,
            start: Instant::now(),
            lock_wait_base: LOCK_WAIT_MS.load(Ordering::Relaxed),
        }
    }

    /// Append one complete record as a single line. Errors are swallowed (logged
    /// to stderr via tracing) — observability never breaks a tool call.
    pub fn append(&self, rec: &OpRecord) {
        let mut line = match serde_json::to_string(rec) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("ops.log serialize failed: {e}");
                return;
            }
        };
        line.push('\n');
        write_line(&self.path, self.max_bytes, &line);
        // Mirror into the machine-global log too (best-effort; never breaks a call).
        if let Some(g) = &self.global_path {
            write_line(g, self.max_bytes, &line);
        }
    }

    /// Append a verbose decision trail for `rec` to `explain.log` (no-op unless
    /// explain mode is enabled).
    pub fn append_explain(&self, rec: &OpRecord, trail: &str) {
        if !self.explain_enabled {
            return;
        }
        let store_ref = rec.store_ref.as_deref().unwrap_or("-");
        let block = format!(
            "[{ts}] {tool} {agent} outcome={outcome} dur={dur}ms lock_wait={lw}ms\n  \
             input: {input}\n  \
             raw_bytes_in={raw} bytes_returned={ret} tokens_saved_est={saved} store_ref={sref}\n  \
             trail: {trail}\n",
            ts = rec.ts,
            tool = rec.tool,
            agent = rec.agent_id,
            outcome = rec.outcome,
            dur = rec.duration_ms,
            lw = rec.lock_wait_ms,
            input = rec.input_summary,
            raw = rec.raw_bytes_in,
            ret = rec.bytes_returned,
            saved = rec.tokens_saved_est,
            sref = store_ref,
            trail = trail,
        );
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.explain_path)
        {
            Ok(mut f) => {
                let _ = f.write_all(block.as_bytes());
            }
            Err(e) => tracing::warn!("explain.log open failed: {e}"),
        }
    }
}

/// Append one line to `path`, rotating first if needed. Best-effort: a failed write
/// is logged, never propagated, so observability cannot break a tool call. One write
/// of the whole line keeps parallel-process appends from interleaving into corrupt
/// lines (O_APPEND positions at EOF atomically).
fn write_line(path: &Path, max_bytes: u64, line: &str) {
    rotate_if_needed_at(path, max_bytes, line.len() as u64);
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!("ops.log write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("ops.log open failed: {e}"),
    }
}

/// Rotate `path` to `path.1` if appending `next_len` bytes would push it past the
/// cap. Best-effort under concurrency: `rename` is atomic, so the worst case is a
/// slightly-over-cap file or a redundant rotation, never a corrupt one.
fn rotate_if_needed_at(path: &Path, max_bytes: u64, next_len: u64) {
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() + next_len > max_bytes {
            let _ = std::fs::rename(path, rotated_path(path));
        }
    }
}

/// The machine-global `ops.log` mirror for a per-repo `local` log: `home_root()`
/// joined with `ops.log`, unless that equals `local` (the data dir already is home).
/// In test builds the mirror is suppressed unless `LENS_HOME` is set, so unit
/// tests never append to the developer's real `~/.lens/ops.log`.
fn global_ops_path(local: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        std::env::var_os("LENS_HOME")?;
    }
    crate::rtk::home_root()
        .map(|h| h.join("ops.log"))
        .filter(|g| g != local)
}

/// `ops.log` -> `ops.log.1` (append a suffix; `with_extension` would mangle it).
fn rotated_path(path: &Path) -> PathBuf {
    let mut s = path.to_path_buf().into_os_string();
    s.push(".1");
    PathBuf::from(s)
}

fn explain_env() -> bool {
    matches!(
        std::env::var("LENS_EXPLAIN").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}

// ---------------------------------------------------------------------------
// Per-op handle: times the op, then writes its record
// ---------------------------------------------------------------------------

/// A live operation being timed. Created by [`OpLog::start`]; consumed by
/// [`OpHandle::finish`], which stamps duration + lock-wait and appends the record.
pub struct OpHandle {
    log: OpLog,
    tool: &'static str,
    input_summary: Value,
    start: Instant,
    lock_wait_base: u64,
}

impl OpHandle {
    /// Finalize the op: compute duration/lock-wait/token savings, append the
    /// JSONL record, and (if enabled) the explain trail.
    pub fn finish(
        self,
        raw_bytes_in: u64,
        bytes_returned: u64,
        store_ref: Option<String>,
        outcome: &str,
        note: impl Into<String>,
        explain: Option<String>,
    ) {
        let duration_ms = self.start.elapsed().as_millis() as u64;
        let lock_wait_ms = LOCK_WAIT_MS
            .load(Ordering::Relaxed)
            .saturating_sub(self.lock_wait_base);
        let tokens_saved_est = ((raw_bytes_in as i64 - bytes_returned as i64).max(0)) / 4;
        let pid = std::process::id();
        let rec = OpRecord {
            ts: iso8601_now(),
            // Prefer the session id the hook published; fall back to the env var
            // (tests / explicit overrides), else None.
            session_id: self
                .log
                .current_session()
                .or_else(|| std::env::var("LENS_SESSION_ID").ok()),
            agent_id: std::env::var("LENS_AGENT_ID").unwrap_or_else(|_| format!("pid-{pid}")),
            pid,
            tool: self.tool.to_string(),
            input_summary: self.input_summary,
            raw_bytes_in,
            bytes_returned,
            tokens_saved_est,
            store_ref,
            duration_ms,
            lock_wait_ms,
            outcome: outcome.to_string(),
            note: note.into(),
        };
        self.log.append(&rec);
        if let Some(trail) = explain {
            self.log.append_explain(&rec, &trail);
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite concurrency plumbing (shared by every lens DB)
// ---------------------------------------------------------------------------

/// Configure a freshly-opened lens SQLite connection for safe concurrent
/// use: WAL journaling (concurrent readers don't block the single writer) plus a
/// busy handler that retries on contention instead of erroring with "database is
/// locked", while accumulating waited time into [`LOCK_WAIT_MS`].
pub fn configure_conn(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    conn.busy_handler(Some(busy_handler))?;
    // execute_batch tolerates the row PRAGMA journal_mode returns.
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    Ok(())
}

/// Busy handler: sleep ~1 ms per retry (≈10 s ceiling, matching the previous
/// busy_timeout) and record the wait. `count` is the number of prior retries for
/// this lock event. Returning `false` gives up (surfaces SQLITE_BUSY).
fn busy_handler(count: i32) -> bool {
    if count > 10_000 {
        return false;
    }
    std::thread::sleep(Duration::from_millis(1));
    LOCK_WAIT_MS.fetch_add(1, Ordering::Relaxed);
    true
}

// ---------------------------------------------------------------------------
// Small shared helpers
// ---------------------------------------------------------------------------

/// Serialized byte length of a value (what it costs in context if returned).
pub fn json_len<T: Serialize>(v: &T) -> u64 {
    serde_json::to_vec(v).map(|b| b.len() as u64).unwrap_or(0)
}

/// Resolve the data dir the way the server does: `$LENS_DIR`, else
/// `<cwd>/.lens`. Used by the read-only CLI subcommands.
pub fn data_dir() -> PathBuf {
    match std::env::var_os("LENS_DIR") {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".lens"),
    }
}

/// ISO-8601 (`...Z`, millis = 0) for a unix-seconds instant. Used as a lower-bound
/// cutoff to compare against op-record `ts` strings (which sort chronologically).
pub fn iso8601_secs(secs: i64) -> String {
    iso8601(secs, 0)
}

/// Current UTC time as ISO-8601 with millisecond precision (`...Z`).
pub fn iso8601_now() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso8601(now.as_secs() as i64, now.subsec_millis())
}

fn iso8601(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}Z")
}

/// Civil date (year, month, day) from days since the Unix epoch. Howard
/// Hinnant's `civil_from_days` (proleptic Gregorian; valid for any epoch day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m as u32, d)
}

/// Parse an ISO-8601 `YYYY-MM-DDTHH:MM:SS(.mmm)Z` string (as produced by
/// [`iso8601`]) back to unix seconds. Millis are ignored. Returns `None` on
/// malformed input.
pub fn iso8601_to_secs(s: &str) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)? as u32, num(8, 10)? as u32);
    let (hh, mm, ss) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    Some(days_from_civil(y, mo, d) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Days since the Unix epoch for a civil date. Inverse of [`civil_from_days`]
/// (Howard Hinnant; proleptic Gregorian).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn iso8601_known_epochs() {
        assert_eq!(iso8601(0, 0), "1970-01-01T00:00:00.000Z");
        // 1_700_000_000 == 2023-11-14T22:13:20 UTC.
        assert_eq!(iso8601(1_700_000_000, 7), "2023-11-14T22:13:20.007Z");
    }

    #[test]
    fn append_writes_one_parseable_line_per_op() {
        let dir = tempdir().unwrap();
        let log = OpLog::open(dir.path());
        log.start("lens_run", json!({"language": "python", "code_bytes": 10}))
            .finish(8000, 100, Some("abc".into()), "ok", "stored", None);
        let raw = std::fs::read_to_string(dir.path().join("ops.log")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 1);
        let rec: OpRecord = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(rec.tool, "lens_run");
        assert_eq!(rec.raw_bytes_in, 8000);
        assert_eq!(rec.bytes_returned, 100);
        assert_eq!(rec.tokens_saved_est, (8000 - 100) / 4);
        assert_eq!(rec.store_ref.as_deref(), Some("abc"));
    }

    #[test]
    fn finish_uses_published_current_session() {
        // The hook publishes the active session to <dir>/current_session; finish()
        // must stamp the op record with it (this is the IPC the server relies on).
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("current_session"), "abc-123\n").unwrap();
        let log = OpLog::open(dir.path());
        log.start("lens_run", json!({}))
            .finish(10, 5, None, "ok", "", None);
        let raw = std::fs::read_to_string(dir.path().join("ops.log")).unwrap();
        let rec: OpRecord = serde_json::from_str(raw.lines().next().unwrap()).unwrap();
        assert_eq!(rec.session_id.as_deref(), Some("abc-123"));
    }

    #[test]
    fn rotation_at_cap_moves_to_dot_one() {
        let dir = tempdir().unwrap();
        std::env::set_var("LENS_OPS_LOG_MAX", "400");
        let log = OpLog::open(dir.path());
        std::env::remove_var("LENS_OPS_LOG_MAX");
        for _ in 0..50 {
            log.start("lens_search", json!({"queries": 1}))
                .finish(10, 10, None, "ok", "", None);
        }
        let main = dir.path().join("ops.log");
        let rotated = dir.path().join("ops.log.1");
        assert!(rotated.exists(), "expected rotation to ops.log.1");
        assert!(std::fs::metadata(&main).unwrap().len() <= 400 + 256);
    }

    #[test]
    fn explain_log_only_when_enabled() {
        let dir = tempdir().unwrap();
        // Off by default.
        let log = OpLog::open(dir.path());
        log.start("lens_run", json!({}))
            .finish(0, 0, None, "ok", "", Some("trail".into()));
        assert!(!dir.path().join("explain.log").exists());

        std::env::set_var("LENS_EXPLAIN", "1");
        let log = OpLog::open(dir.path());
        std::env::remove_var("LENS_EXPLAIN");
        let trail = log.explain(|| "decision: offloaded 47KB".to_string());
        log.start("lens_run", json!({}))
            .finish(48000, 100, Some("r".into()), "ok", "", trail);
        let explain = std::fs::read_to_string(dir.path().join("explain.log")).unwrap();
        assert!(explain.contains("decision: offloaded 47KB"));
    }

    #[test]
    fn append_mirrors_into_global_home() {
        // home_root() is env-driven and process-global; serialize with other mutators.
        let _g = crate::rtk::env_test_lock();
        let home = tempdir().unwrap();
        std::env::set_var("LENS_HOME", home.path());
        let data = tempdir().unwrap();
        OpLog::open(data.path())
            .start("lens_run", json!({}))
            .finish(8000, 100, Some("r".into()), "ok", "", None);
        std::env::remove_var("LENS_HOME");
        // The per-repo log carries the record...
        let local = std::fs::read_to_string(data.path().join("ops.log")).unwrap();
        assert!(local.contains("lens_run"));
        // ...and so does the machine-global mirror under home_root().
        let global = std::fs::read_to_string(home.path().join("ops.log")).unwrap();
        assert!(global.contains("lens_run"));
    }
}
