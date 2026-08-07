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

pub mod credit;
pub mod dashboard;
pub mod pricing;
mod pricing_catalog;
pub mod stats;
pub mod tui;
pub mod usage;
pub mod value_model;
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
    /// How `raw_bytes_in` was credited toward `tokens_saved_est`:
    /// `"context_bound" | "volunteered" | "neutral"` (see `credit::CreditClass`).
    /// `#[serde(default)]` so pre-existing log lines (written before this field
    /// existed) still parse, just with an empty string.
    #[serde(default)]
    pub credit_class: String,
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
    // Test/bench harnesses must never mirror into the real ~/.lens ledger; doing
    // so pollutes the dashboard with fixture ops (a 24-worker concurrency test
    // alone leaked ~58M "saved" tokens). Unit tests are gated by cfg(test);
    // integration tests link the lib in normal mode, so they and every other
    // cargo-launched process opt out via LENS_NO_GLOBAL_MIRROR (set for all of
    // cargo in .cargo/config.toml). The installed server runs the binary
    // directly, not via cargo, so it still mirrors.
    if std::env::var_os("LENS_NO_GLOBAL_MIRROR").is_some() {
        return None;
    }
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
        let class = credit::classify(self.tool, &self.input_summary, raw_bytes_in);
        let credited = credit::credited_raw(class, raw_bytes_in);
        let tokens_saved_est = ((credited as i64 - bytes_returned as i64).max(0)) / 4;
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
            credit_class: class.as_str().to_string(),
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

/// Default busy-handler retry ceiling (~10 s at 1 ms/retry), matching the previous
/// `busy_timeout`. A fresh multi-repo `lens index` can hold the write lock for the
/// whole walk; overriding this via `LENS_BUSY_MS` lets a starved second writer fail
/// fast instead of blocking ~10 s on the busy handler.
const DEFAULT_BUSY_CEILING: u64 = 10_000;

/// Busy-handler retry ceiling, in ~1 ms units (so effectively a millisecond budget).
/// Process-global because rusqlite's busy handler is a bare `fn` that cannot capture
/// state; re-seeded from `LENS_BUSY_MS` (or the default) on every [`configure_conn`].
pub static BUSY_CEILING_MS: AtomicU64 = AtomicU64::new(DEFAULT_BUSY_CEILING);

/// Configure a freshly-opened lens SQLite connection for safe concurrent
/// use: WAL journaling (concurrent readers don't block the single writer) plus a
/// busy handler that retries on contention instead of erroring with "database is
/// locked", while accumulating waited time into [`LOCK_WAIT_MS`].
pub fn configure_conn(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // Cap how long a writer retries on a locked DB before giving up (default ~10 s).
    // Read here, once per connection, rather than on every 1 ms retry in the handler.
    let ceiling = std::env::var("LENS_BUSY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BUSY_CEILING);
    BUSY_CEILING_MS.store(ceiling, Ordering::Relaxed);
    conn.busy_handler(Some(busy_handler))?;
    // WAL keeps concurrent readers from blocking the single writer. synchronous=NORMAL
    // under WAL fsyncs on checkpoint instead of on every commit (bench_tuning W2:
    // ~12-18% faster per-commit re-index); the only cost is losing the LAST committed
    // transaction on power loss, which cannot corrupt the DB. Every lens DB is a
    // rebuildable cache (index_path / discover rebuild from source), so that trade is
    // acceptable. temp_store is left at default: MEMORY measured net-neutral (W2), not
    // worth pinning. execute_batch tolerates the row PRAGMA journal_mode returns.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    Ok(())
}

/// Busy handler: sleep ~1 ms per retry and record the wait, giving up once retries
/// pass [`BUSY_CEILING_MS`] (so the total wait is ~that many ms). `count` is the
/// number of prior retries for this lock event. Returning `false` gives up
/// (surfaces SQLITE_BUSY).
fn busy_handler(count: i32) -> bool {
    if count as u64 > BUSY_CEILING_MS.load(Ordering::Relaxed) {
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

/// Accurate token count for `text` via an offline BPE tokenizer (o200k_base, the
/// GPT-4o family; the vocab is embedded in `tiktoken-rs`, so no network). Replaces
/// the bytes/4 heuristic wherever the actual text is in hand (the benchmark arms,
/// the token-estimate gate). The cumulative byte-savings counters in
/// [`OpHandle::finish`] stay a bytes ratio because they only ever see byte counts
/// of data that never entered context, never the text itself.
pub fn count_tokens(text: &str) -> usize {
    use std::sync::OnceLock;
    static BPE: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();
    let bpe = BPE.get_or_init(|| tiktoken_rs::o200k_base().expect("load o200k_base BPE"));
    bpe.encode_ordinary(text).len()
}

/// Stable identity for a project root: blake3 of its canonicalized path string,
/// first 16 hex. Canonicalizing is what keeps the identity stable across the
/// spellings the different planes see — the hook's cwd-resolved path, a
/// `lens warmup ../proj` argument, a symlinked checkout — which is the whole
/// point of a shared seam: hook, server and warmup must land on ONE dir or the
/// rails read the server as down. A root that cannot be canonicalized (it does
/// not exist yet) hashes the path as given.
///
/// The recipe is byte-identical to the one `server::unscoped_data_dir_from` and
/// `discovery`'s probe cache hash with today, so pointing those call sites at
/// this helper leaves every `~/.lens/unscoped/<hash>` dir already on disk exactly
/// where it is.
pub fn project_hash(root: &Path) -> String {
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex()[..16].to_string()
}

/// Where `root`'s persistent state lives — the one resolver every plane (server,
/// hook, warmup, federation, dashboard, status) shares. Four rules, in order:
///   1. `$LENS_DIR`, set and non-empty: an explicit pin always wins (the bench
///      and test harnesses set it and depend on state landing exactly there);
///   2. `<root>/.lens` already holding real artifacts: keep-if-present, so an
///      index built before central storage never moves out from under a repo;
///   3. `LENS_CENTRAL_STORE=0`: the pre-central layout, `<root>/.lens`;
///   4. otherwise `<home>/projects/<project_hash>` — a fresh index leaves no
///      droppings in the tree at all.
///
/// PURE: resolution only ever stats, never creates. `lens status` resolves a
/// never-indexed directory read-only and must leave it exactly as it found it.
pub fn data_dir_for(root: &Path) -> PathBuf {
    let lens_dir = std::env::var_os("LENS_DIR")
        .filter(|d| !d.is_empty())
        .map(PathBuf::from);
    let central = std::env::var("LENS_CENTRAL_STORE").ok();
    data_dir_for_from(
        root,
        lens_dir.as_deref(),
        central.as_deref(),
        crate::rtk::home_root(),
    )
}

/// [`data_dir_for`] with the environment injected, so every rule is exercisable
/// without mutating process-global env vars (same split, and same reason, as
/// `index::build_threads_from` / `server::unscoped_data_dir_from`; `$LENS_DIR` in
/// particular is read unguarded by `session::hook`'s own tests, so setting it
/// in-process would corrupt siblings).
fn data_dir_for_from(
    root: &Path,
    lens_dir: Option<&Path>,
    central_store: Option<&str>,
    home: Option<PathBuf>,
) -> PathBuf {
    if let Some(pinned) = lens_dir {
        return pinned.to_path_buf();
    }
    let in_tree = root.join(".lens");
    // Same artifact predicate as `discovery::is_project_root_at`'s `.lens` arm: a
    // bare `.lens/` (stray heartbeats and nothing else) is not an index, so one
    // dropping cannot opt a repo out of central storage for good.
    if in_tree.join("index.db").exists() || in_tree.join("graph.json").exists() {
        return in_tree;
    }
    if central_store == Some("0") {
        return in_tree;
    }
    match home {
        Some(h) => h.join("projects").join(project_hash(root)),
        // No home at all: fall back to the pre-central layout rather than
        // scattering state into a temp dir the user could never find (and that
        // `lens clean` would never look in).
        None => in_tree,
    }
}

/// Resolve the data dir for the current directory (see [`data_dir_for`]). Used by
/// the read-only CLI subcommands.
pub fn data_dir() -> PathBuf {
    data_dir_for(&std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Record `root -> data_dir` in the machine-global registry
/// (`<home>/registry.tsv`), the index `lens clean` reads to find every data dir
/// lens ever opened for write — without it, a central dir whose root is long gone
/// is unattributable garbage. Called from the write paths only; resolution itself
/// stays pure.
///
/// Best-effort by design: a missed line costs `lens clean` a blind spot, never a
/// failed build, so every error is swallowed. Skipped entirely under
/// `LENS_NO_GLOBAL_MIRROR`, the same isolation switch the ops mirror and the probe
/// cache respect, so test and bench runs never touch the real `~/.lens`.
pub fn record_registry(root: &Path, data_dir: &Path) {
    record_registry_from(
        root,
        data_dir,
        std::env::var_os("LENS_NO_GLOBAL_MIRROR").is_some(),
        crate::rtk::home_root(),
    );
}

/// [`record_registry`] with the environment injected (same `_from` split as
/// [`data_dir_for_from`]).
fn record_registry_from(root: &Path, data_dir: &Path, no_mirror: bool, home: Option<PathBuf>) {
    if no_mirror {
        return;
    }
    let Some(home) = home else {
        return;
    };
    let entry = format!("{}\t{}", root.display(), data_dir.display());
    let path = home.join("registry.tsv");
    // Dedup is a read-then-append under no lock: two processes racing the same new
    // root can both miss and write the line twice, which readers dedup. Holding a
    // lock across every data-dir open would cost far more than that is worth.
    if std::fs::read_to_string(&path).is_ok_and(|s| s.lines().any(|l| l == entry)) {
        return;
    }
    let _ = std::fs::create_dir_all(&home);
    // One `write_all` of the whole line on an O_APPEND handle: appends position at
    // EOF atomically, so parallel writers never interleave into a corrupt line
    // (same discipline as `write_line`; `writeln!` would not — it can split the
    // line across several writes).
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(format!("{entry}\n").as_bytes());
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

        // A volunteered op (data file handed to the darkroom, not intercepted
        // context) is credited only up to the plausible-slice floor, while a
        // context-bound op (source file, stands in for a would-be Read) is
        // credited in full — both derived by the shared classifier in finish().
        let raw_huge = 5 * 1024 * 1024u64;
        let returned = 100u64;
        log.start("lens_run_file", json!({"path": "x.log"}))
            .finish(raw_huge, returned, None, "ok", "", None);
        log.start("lens_run_file", json!({"path": "src/main.rs"}))
            .finish(raw_huge, returned, None, "ok", "", None);
        let raw = std::fs::read_to_string(dir.path().join("ops.log")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3);
        let log_rec: OpRecord = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(log_rec.credit_class, "volunteered");
        assert_eq!(log_rec.tokens_saved_est, (32_768i64 - returned as i64) / 4);
        let rs_rec: OpRecord = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(rs_rec.credit_class, "context_bound");
        assert_eq!(
            rs_rec.tokens_saved_est,
            (raw_huge as i64 - returned as i64) / 4
        );
    }

    #[test]
    fn global_mirror_opt_out_suppresses_mirror() {
        // home_root()/env are process-global; serialize with the other mutators.
        let _g = crate::rtk::env_test_lock();
        let prev_home = std::env::var_os("LENS_HOME");
        let prev_flag = std::env::var_os("LENS_NO_GLOBAL_MIRROR");
        // LENS_HOME opens the cfg(test) mirror gate; the runtime opt-out (set for
        // integration tests + cargo runs via .cargo/config.toml) must close it, so
        // fixture ops never reach the real ledger.
        let home = tempdir().unwrap();
        let local = home.path().join("proj/.lens/ops.log");
        std::env::set_var("LENS_HOME", home.path());
        std::env::remove_var("LENS_NO_GLOBAL_MIRROR");
        let opted_in = global_ops_path(&local);
        std::env::set_var("LENS_NO_GLOBAL_MIRROR", "1");
        let opted_out = global_ops_path(&local);
        match prev_home {
            Some(v) => std::env::set_var("LENS_HOME", v),
            None => std::env::remove_var("LENS_HOME"),
        }
        match prev_flag {
            Some(v) => std::env::set_var("LENS_NO_GLOBAL_MIRROR", v),
            None => std::env::remove_var("LENS_NO_GLOBAL_MIRROR"),
        }
        assert!(opted_in.is_some(), "mirror on when opted in");
        assert!(opted_out.is_none(), "opt-out suppresses mirror");
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

    // ── data-dir resolution: the seam every plane agrees on ──────────────────
    //
    // Every case drives `data_dir_for_from` / `record_registry_from` directly:
    // `$LENS_DIR` is read unguarded by `session::hook`'s tests, so mutating it
    // in-process to exercise the wrapper would corrupt siblings (the same reason
    // `server`'s `unscoped_data_dir_from` tests keep to the injected core).

    /// Rule 1 outranks every other rule: a harness that pins `$LENS_DIR` gets
    /// exactly that dir, legacy artifacts and kill switch notwithstanding. This
    /// is what keeps the three bench binaries meaningful.
    #[test]
    fn lens_dir_pin_wins_over_every_other_rule() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".lens")).unwrap();
        std::fs::write(root.join(".lens").join("index.db"), b"legacy").unwrap();
        let pinned = tmp.path().join("pinned");
        let home = tmp.path().join("home");

        for central in [None, Some("0"), Some("1")] {
            assert_eq!(
                data_dir_for_from(&root, Some(pinned.as_path()), central, Some(home.clone())),
                pinned,
                "$LENS_DIR must win with LENS_CENTRAL_STORE={central:?}"
            );
        }
    }

    /// Rule 2, keep-if-present: a repo indexed before central storage keeps using
    /// its in-tree `.lens`, so no existing index ever moves. Both artifacts
    /// qualify, matching `discovery`'s marker predicate exactly.
    #[test]
    fn legacy_in_tree_artifacts_keep_the_tree_dir() {
        for artifact in ["index.db", "graph.json"] {
            let tmp = tempdir().unwrap();
            let root = tmp.path().join("proj");
            std::fs::create_dir_all(root.join(".lens")).unwrap();
            std::fs::write(root.join(".lens").join(artifact), b"x").unwrap();
            assert_eq!(
                data_dir_for_from(&root, None, None, Some(tmp.path().join("home"))),
                root.join(".lens"),
                "a legacy {artifact} must pin the in-tree dir"
            );
        }
    }

    /// A bare `.lens/` (stray heartbeats, no index) is not an artifact: it must
    /// not pin the tree, or one dropping would opt a repo out of central storage
    /// permanently.
    #[test]
    fn bare_dot_lens_does_not_pin_the_tree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(root.join(".lens").join("heartbeats")).unwrap();
        let home = tmp.path().join("home");

        let got = data_dir_for_from(&root, None, None, Some(home.clone()));

        assert_eq!(got, home.join("projects").join(project_hash(&root)));
    }

    /// Rule 4: a fresh root lands under the global home, out of the tree, and two
    /// roots never share one data dir.
    #[test]
    fn fresh_roots_map_to_distinct_central_dirs() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let (a, b) = (tmp.path().join("a"), tmp.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        let da = data_dir_for_from(&a, None, None, Some(home.clone()));
        let db = data_dir_for_from(&b, None, None, Some(home.clone()));

        assert_eq!(da, home.join("projects").join(project_hash(&a)));
        assert!(!da.starts_with(&a), "central storage leaves the tree alone");
        assert_ne!(da, db, "two roots must not share one data dir");
    }

    /// Rule 3, the kill switch: `LENS_CENTRAL_STORE=0` restores the pre-central
    /// layout for a root with no legacy artifacts at all. Only `0` opts out.
    #[test]
    fn central_store_kill_switch_restores_in_tree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let home = tmp.path().join("home");

        assert_eq!(
            data_dir_for_from(&root, None, Some("0"), Some(home.clone())),
            root.join(".lens")
        );
        assert_eq!(
            data_dir_for_from(&root, None, Some("1"), Some(home.clone())),
            home.join("projects").join(project_hash(&root))
        );
    }

    /// No home to hold a central dir: fall back to the pre-central layout rather
    /// than scattering state somewhere the user could never find it.
    #[test]
    fn homeless_process_falls_back_to_in_tree() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();

        assert_eq!(
            data_dir_for_from(&root, None, None, None),
            root.join(".lens")
        );
    }

    /// Resolution is PURE: `lens status` resolves a never-indexed directory
    /// read-only, so a resolve may not add a single entry to the tree OR to the
    /// global home — not even the dir it just named.
    #[test]
    fn resolving_creates_nothing_on_disk() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let home = tmp.path().join("home");

        let central = data_dir_for_from(&root, None, None, Some(home.clone()));
        let in_tree = data_dir_for_from(&root, None, Some("0"), Some(home.clone()));

        assert!(!central.exists(), "resolver created {}", central.display());
        assert!(!home.exists(), "resolver created the global home");
        assert!(!in_tree.exists(), "resolver created the in-tree dir");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            0,
            "resolver left something in the tree"
        );
    }

    /// `project_hash` must stay byte-identical to the recipe
    /// `server::unscoped_data_dir_from` (and `discovery`'s probe cache) hash with
    /// today — blake3 of the path string, first 16 hex — or every
    /// `~/.lens/unscoped/<hash>` dir already on disk would move the moment those
    /// call sites switch onto this helper.
    #[test]
    fn project_hash_matches_the_existing_unscoped_recipe() {
        // The literal the server's own unscoped test hashes. It does not exist,
        // so canonicalization falls back to the path as given.
        let literal = Path::new("/tmp/some/markerless/tree");
        let want = blake3::hash(literal.to_string_lossy().as_bytes()).to_hex()[..16].to_string();
        assert_eq!(project_hash(literal), want);
        assert_eq!(want.len(), 16);

        // And a real, already-canonical root: canonicalization is the identity
        // there, so that hash is unchanged too.
        let tmp = tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let want = blake3::hash(root.to_string_lossy().as_bytes()).to_hex()[..16].to_string();
        assert_eq!(project_hash(&root), want);
    }

    /// The one deliberate delta from hashing the raw path: two spellings of the
    /// same root resolve to ONE data dir. The hook sees a cwd-resolved path and
    /// `lens warmup` sees whatever the user typed; if those hashed differently
    /// the rails would read a live server as down.
    #[cfg(unix)]
    #[test]
    fn project_hash_is_stable_across_path_spellings() {
        let tmp = tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let real = base.join("proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(project_hash(&real), project_hash(&link));
    }

    /// The registry is what `lens clean` reads: one line per root/data-dir pair,
    /// appended once no matter how many times the data dir is opened.
    #[test]
    fn registry_appends_one_line_per_distinct_pair() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        let data = Path::new("/home/u/.lens/projects/aa");
        let record = |root: &str| {
            record_registry_from(Path::new(root), data, false, Some(home.clone()));
        };

        record("/repos/a");
        record("/repos/a");
        assert_eq!(
            std::fs::read_to_string(home.join("registry.tsv")).unwrap(),
            "/repos/a\t/home/u/.lens/projects/aa\n",
            "an identical line must not be appended twice"
        );

        record("/repos/b");
        let raw = std::fs::read_to_string(home.join("registry.tsv")).unwrap();
        assert_eq!(raw.lines().count(), 2, "a distinct root is a new line");
        assert!(raw.ends_with("/repos/b\t/home/u/.lens/projects/aa\n"));
    }

    /// Bench and test isolation: under `LENS_NO_GLOBAL_MIRROR` the registry is
    /// skipped entirely — not even the home dir is created.
    #[test]
    fn registry_skipped_under_global_mirror_opt_out() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");

        record_registry_from(
            Path::new("/repos/a"),
            Path::new("/d"),
            true,
            Some(home.clone()),
        );

        assert!(!home.exists(), "opt-out must not touch the global home");
    }

    /// Recording is best-effort: an unwritable home (here: a regular file where
    /// the home dir should be) is swallowed, never a panic in a build path.
    #[test]
    fn registry_write_failure_is_silent() {
        let tmp = tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::write(&home, b"not a dir").unwrap();

        record_registry_from(Path::new("/repos/a"), Path::new("/d"), false, Some(home));
        record_registry_from(Path::new("/repos/a"), Path::new("/d"), false, None);
    }

    #[test]
    fn append_mirrors_into_global_home() {
        // home_root() is env-driven and process-global; serialize with other mutators.
        let _g = crate::rtk::env_test_lock();
        // .cargo/config.toml sets the opt-out for every cargo run; clear it so this
        // test exercises the real mirror path, then restore it for other tests.
        let prev_flag = std::env::var_os("LENS_NO_GLOBAL_MIRROR");
        std::env::remove_var("LENS_NO_GLOBAL_MIRROR");
        let home = tempdir().unwrap();
        std::env::set_var("LENS_HOME", home.path());
        let data = tempdir().unwrap();
        OpLog::open(data.path())
            .start("lens_run", json!({}))
            .finish(8000, 100, Some("r".into()), "ok", "", None);
        std::env::remove_var("LENS_HOME");
        let local = std::fs::read_to_string(data.path().join("ops.log")).unwrap();
        let global = std::fs::read_to_string(home.path().join("ops.log")).unwrap();
        if let Some(v) = prev_flag {
            std::env::set_var("LENS_NO_GLOBAL_MIRROR", v);
        }
        // The per-repo log carries the record, and so does the machine-global mirror.
        assert!(local.contains("lens_run"));
        assert!(global.contains("lens_run"));
    }
}
