//! SQLite event store for session continuity (`session.db`).
//!
//! Mirrors the store/index concurrency convention: one connection per operation
//! with a busy timeout, safe for the short-lived hook subprocesses that each
//! open their own handle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::Connection;

use super::Event;

/// A persisted resume snapshot for a (session, project).
#[derive(Debug, Clone)]
pub struct Resume {
    pub snapshot: String,
    pub event_count: i64,
    pub consumed: bool,
}

/// Read-only summary of the lifecycle activity captured by the session hooks
/// (built-in tool use, prompts, errors). This is the "first plane" — distinct
/// from lens MCP tool usage, which the op log measures.
#[derive(Debug, Default, Clone)]
pub struct Activity {
    pub total_events: i64,
    pub sessions: i64,
    /// Most recent event timestamp (unix seconds), if any.
    pub last_ts: Option<i64>,
    /// `(category, count)`, highest count first.
    pub by_category: Vec<(String, i64)>,
    /// `(source_hook, count)`, highest count first.
    pub by_hook: Vec<(String, i64)>,
}

/// Handle to the session event store.
#[derive(Clone)]
pub struct SessionStore {
    db_path: PathBuf,
    /// Machine-global mirror under `home_root()`, so the dashboard's global scope
    /// can total built-in-tool activity across every repo and launch profile —
    /// symmetric with [`crate::obs::OpLog`]'s `ops.log` mirror. `None` when this
    /// data dir already is the global home (no self-mirror) or none is resolvable.
    /// Only the activity-plane writes (`session_events`) mirror; resume/compaction
    /// state stays repo-local, where the MCP server and `/resume` read it.
    global: Option<Box<SessionStore>>,
}

impl SessionStore {
    /// Open (creating if needed) the session store under `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating data dir {}", dir.display()))?;
        let db_path = dir.join("session.db");
        // Best-effort global mirror: a failure to open it must never break the
        // primary store (and thus a hook). Mirrors the OpLog mirror's tolerance.
        let global = global_session_path(&db_path).and_then(|g| {
            if let Some(parent) = g.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let m = SessionStore {
                db_path: g,
                global: None,
            };
            m.init().ok().map(|_| Box::new(m))
        });
        let store = SessionStore { db_path, global };
        store.init()?;
        Ok(store)
    }

    fn conn(&self) -> Result<Connection> {
        let conn = Connection::open(&self.db_path)
            .with_context(|| format!("opening session db {}", self.db_path.display()))?;
        crate::obs::configure_conn(&conn)?;
        Ok(conn)
    }

    fn init(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS session_events (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id  TEXT NOT NULL,
                project     TEXT NOT NULL,
                timestamp   INTEGER NOT NULL,
                category    TEXT NOT NULL,
                priority    INTEGER NOT NULL,
                payload     TEXT NOT NULL,
                source_hook TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_events_session ON session_events(session_id);
             CREATE INDEX IF NOT EXISTS idx_events_project ON session_events(project);
             CREATE INDEX IF NOT EXISTS idx_events_session_ts
                 ON session_events(session_id, timestamp);
             CREATE TABLE IF NOT EXISTS session_meta (
                session_id    TEXT PRIMARY KEY,
                project       TEXT NOT NULL,
                started_at    INTEGER NOT NULL,
                compact_count INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS session_resume (
                session_id  TEXT NOT NULL,
                project     TEXT NOT NULL,
                snapshot    TEXT NOT NULL,
                event_count INTEGER NOT NULL,
                created_at  INTEGER NOT NULL,
                consumed    INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, project)
             );
             CREATE TABLE IF NOT EXISTS project_memory (
                project    TEXT NOT NULL,
                category   TEXT NOT NULL,
                text       TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (project, category, text)
             );",
        )?;
        Ok(())
    }

    /// Record session metadata if absent (idempotent).
    pub fn ensure_session(&self, session_id: &str, project: &str, started_at: i64) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT OR IGNORE INTO session_meta (session_id, project, started_at)
             VALUES (?1, ?2, ?3)",
            rusqlite::params![session_id, project, started_at],
        )?;
        if let Some(g) = &self.global {
            let _ = g.ensure_session(session_id, project, started_at);
        }
        Ok(())
    }

    /// Append events in one transaction.
    pub fn insert_events(&self, events: &[Event]) -> Result<usize> {
        let mut conn = self.conn()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO session_events
                   (session_id, project, timestamp, category, priority, payload, source_hook)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for e in events {
                stmt.execute(rusqlite::params![
                    e.session_id,
                    e.project,
                    e.timestamp,
                    e.category,
                    e.priority,
                    e.payload.to_string(),
                    e.source_hook,
                ])?;
            }
        }
        // Mirror durable items (decisions, constraints, rejected approaches, rules)
        // into project_memory, which survives the fresh-session event clear so they
        // can be re-injected on the next SessionStart.
        {
            let mut mstmt = tx.prepare_cached(
                "INSERT OR IGNORE INTO project_memory (project, category, text, created_at)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            for e in events {
                if let Some((cat, text)) = memory_item(e) {
                    mstmt.execute(rusqlite::params![e.project, cat, text, e.timestamp])?;
                }
            }
        }
        tx.commit()?;
        if let Some(g) = &self.global {
            let _ = g.insert_events(events);
        }
        Ok(events.len())
    }

    /// All events for a session, oldest first.
    pub fn events_for_session(&self, session_id: &str) -> Result<Vec<Event>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT session_id, project, timestamp, category, priority, payload, source_hook
             FROM session_events WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], Self::row_to_event)?;
        Ok(rows.flatten().collect())
    }

    /// Session events with contradictory entity state resolved at read time.
    ///
    /// The log is append-only, so an entity can flip state across events (a path
    /// touched at t1, deleted at t2). For recovery we want only the latest truth.
    /// Events that share an entity key are collapsed to the most recent by
    /// timestamp (ties keep insertion order, last inserted wins); older
    /// contradictions are dropped from the view but never from the table.
    /// Events with no entity key pass through untouched, in original order.
    pub fn resolved_events_for_session(&self, session_id: &str) -> Result<Vec<Event>> {
        Ok(resolve_contradictions(self.events_for_session(session_id)?))
    }

    /// Count live events for a project (across sessions).
    pub fn count_events_for_project(&self, project: &str) -> Result<i64> {
        let conn = self.conn()?;
        let n: i64 = conn.query_row(
            "SELECT count(*) FROM session_events WHERE project = ?1",
            [project],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// Clear all live events for a project (fresh-session clean slate).
    pub fn clear_project_events(&self, project: &str) -> Result<usize> {
        let conn = self.conn()?;
        let n = conn.execute("DELETE FROM session_events WHERE project = ?1", [project])?;
        if let Some(g) = &self.global {
            let _ = g.clear_project_events(project);
        }
        Ok(n)
    }

    fn row_to_event(row: &rusqlite::Row) -> rusqlite::Result<Event> {
        let payload_str: String = row.get(5)?;
        let payload = serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
        Ok(Event {
            session_id: row.get(0)?,
            project: row.get(1)?,
            timestamp: row.get(2)?,
            category: row.get(3)?,
            priority: row.get(4)?,
            payload,
            source_hook: row.get(6)?,
        })
    }

    /// Store (or replace) the resume snapshot for a session.
    pub fn upsert_resume(
        &self,
        session_id: &str,
        project: &str,
        snapshot: &str,
        event_count: i64,
        created_at: i64,
    ) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "INSERT INTO session_resume (session_id, project, snapshot, event_count, created_at, consumed)
             VALUES (?1, ?2, ?3, ?4, ?5, 0)
             ON CONFLICT(session_id, project) DO UPDATE SET
                snapshot = ?3, event_count = ?4, created_at = ?5, consumed = 0",
            rusqlite::params![session_id, project, snapshot, event_count, created_at],
        )?;
        Ok(())
    }

    /// Bump a session's compaction counter, returning the new value.
    pub fn increment_compact_count(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn()?;
        // Increment and read the new value in one statement. An unknown session
        // matches no row, so RETURNING yields nothing and we report 0 (matching
        // the prior separate-SELECT path's `unwrap_or(0)`).
        let n: i64 = conn
            .query_row(
                "UPDATE session_meta SET compact_count = compact_count + 1
                 WHERE session_id = ?1 RETURNING compact_count",
                [session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n)
    }

    /// Aggregate hook-captured activity, optionally scoped to one session.
    /// Read-only: the `WHERE`/`GROUP BY` run in SQL (covered by
    /// `idx_events_session_ts`), so we never scan rows from other sessions.
    pub fn activity(&self, session: Option<&str>, since: Option<i64>) -> Result<Activity> {
        self.activity_range(session, since, None)
    }

    /// As [`activity`], with an optional exclusive upper time bound (unix seconds) so
    /// the dashboard can scope a bounded `[since, until]` range, not just "since X".
    pub fn activity_range(
        &self,
        session: Option<&str>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<Activity> {
        let conn = self.conn()?;
        // Shared filter, byte-for-byte equivalent to the prior Rust path:
        // `session` -> exact session match; `since` -> keep ts >= cut (the Rust
        // path skipped `ts < cut`, a half-open lower bound); `until` -> keep ts < cut.
        let (where_sql, params) = activity_filter(session, since, until);

        // Scalar plane: total rows, distinct sessions, and the max timestamp
        // (NULL over zero rows -> None, matching the prior `Option` accumulator).
        let (total, sessions, last_ts): (i64, i64, Option<i64>) = conn.query_row(
            &format!(
                "SELECT count(*), count(DISTINCT session_id), max(timestamp)
                 FROM session_events{where_sql}"
            ),
            rusqlite::params_from_iter(params.iter()),
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;

        // Grouped planes. `ORDER BY count DESC, key ASC` reproduces the prior
        // `sort_desc` tie-break exactly: SQLite's default BINARY collation orders
        // TEXT byte-wise, identical to Rust's `str` `Ord`.
        let grouped = |column: &str| -> Result<Vec<(String, i64)>> {
            let mut stmt = conn.prepare(&format!(
                "SELECT {column}, count(*) FROM session_events{where_sql}
                 GROUP BY {column} ORDER BY count(*) DESC, {column} ASC"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };

        Ok(Activity {
            total_events: total,
            sessions,
            last_ts,
            by_category: grouped("category")?,
            by_hook: grouped("source_hook")?,
        })
    }

    /// Distinct project paths seen in this store, most-recently-active first. Backs
    /// the dashboard's repo picker (run against the machine-global store).
    pub fn distinct_projects(&self) -> Result<Vec<String>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT project, max(timestamp) t FROM session_events
             GROUP BY project ORDER BY t DESC",
        )?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Per-bucket event counts over `[start, end]` (unix seconds) split into `n`
    /// equal buckets, optionally scoped to one session. Raw counts, not
    /// cumulative. Mirrors [`activity`]'s session filter. Backs the dashboard's
    /// windowed session-activity sparkline.
    pub fn event_buckets(
        &self,
        session: Option<&str>,
        start: i64,
        end: i64,
        n: usize,
    ) -> Result<Vec<i64>> {
        let conn = self.conn()?;
        // Push the row filter into SQL (covered by `idx_events_session_ts`) so we
        // never scan other sessions or out-of-window rows: `BETWEEN` is the closed
        // interval `[start, end]`, matching the prior `ts < start || ts > end` skip.
        // The float bucket assignment stays in Rust, bit-for-bit identical to the
        // prior path: SQLite integer division would diverge from this f64 clamp at
        // bucket edges (e.g. 28.999.. truncating to 28, where i64 math yields 29).
        let mut where_sql = String::from(" WHERE timestamp BETWEEN ?1 AND ?2");
        let mut params: Vec<&dyn rusqlite::ToSql> = vec![&start, &end];
        if let Some(f) = session.as_ref() {
            where_sql.push_str(" AND session_id = ?3");
            params.push(f);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT timestamp FROM session_events{where_sql}"
        ))?;
        let rows = stmt.query_map(params.as_slice(), |r| r.get::<_, i64>(0))?;
        let span = (end - start).max(1);
        let mut b = vec![0i64; n];
        for ts in rows {
            let ts = ts?;
            let i = (((ts - start) as f64 / span as f64).clamp(0.0, 1.0) * n as f64) as usize;
            b[i.min(n - 1)] += 1;
        }
        Ok(b)
    }

    /// Current compaction count for a session (0 if unknown).
    pub fn compact_count(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn()?;
        let n = conn
            .query_row(
                "SELECT compact_count FROM session_meta WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n)
    }

    /// Fetch the resume row for an exact session.
    pub fn get_resume(&self, session_id: &str, project: &str) -> Result<Option<Resume>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT snapshot, event_count, consumed FROM session_resume
             WHERE session_id = ?1 AND project = ?2",
        )?;
        let mut rows = stmt.query(rusqlite::params![session_id, project])?;
        match rows.next()? {
            Some(row) => Ok(Some(Resume {
                snapshot: row.get(0)?,
                event_count: row.get(1)?,
                consumed: row.get::<_, i64>(2)? != 0,
            })),
            None => Ok(None),
        }
    }

    /// Mark a session's resume row consumed.
    pub fn mark_resume_consumed(&self, session_id: &str, project: &str) -> Result<()> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE session_resume SET consumed = 1 WHERE session_id = ?1 AND project = ?2",
            rusqlite::params![session_id, project],
        )?;
        Ok(())
    }

    /// Claim the most recent unconsumed snapshot for a project, excluding the
    /// given session id (used by /resume, which hands us a fresh session id).
    /// Marks it consumed and returns its snapshot text.
    pub fn claim_latest_unconsumed_resume(
        &self,
        project: &str,
        exclude_session: &str,
    ) -> Result<Option<String>> {
        let conn = self.conn()?;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT session_id, snapshot FROM session_resume
                 WHERE project = ?1 AND consumed = 0 AND session_id != ?2
                 ORDER BY created_at DESC LIMIT 1",
                rusqlite::params![project, exclude_session],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        match row {
            Some((sid, snapshot)) => {
                conn.execute(
                    "UPDATE session_resume SET consumed = 1 WHERE session_id = ?1 AND project = ?2",
                    rusqlite::params![sid, project],
                )?;
                Ok(Some(snapshot))
            }
            None => Ok(None),
        }
    }

    /// Durable project memory (decisions, constraints, rejected approaches, rule
    /// paths) accumulated across every session for `project`, oldest first. Unlike
    /// `session_events` this is never cleared on a fresh-session start, so it can be
    /// re-injected to carry prior decisions into a new session. Returns
    /// `(category, text)` pairs.
    pub fn project_memory(&self, project: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn()?;
        let mut stmt = conn.prepare(
            "SELECT category, text FROM project_memory WHERE project = ?1
             ORDER BY created_at ASC, category ASC, text ASC",
        )?;
        let rows = stmt.query_map([project], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        Ok(rows.flatten().collect())
    }
}

/// Durable memory item for an event, or None if the category is not durable.
/// Decisions/constraints/rejected approaches carry a `text`; rules carry a `path`.
fn memory_item(e: &Event) -> Option<(&'static str, String)> {
    let field = |k: &str| e.payload.get(k).and_then(|v| v.as_str()).map(String::from);
    match e.category.as_str() {
        "decision" => field("text").map(|t| ("decision", t)),
        "constraint" => field("text").map(|t| ("constraint", t)),
        "rejected-approach" => field("text").map(|t| ("rejected-approach", t)),
        "rule" => field("path").map(|t| ("rule", t)),
        _ => None,
    }
}

/// Build the shared `WHERE` clause + bound params for [`SessionStore::activity`].
/// Returns the SQL fragment (empty when unscoped) and the owned positional params.
/// `session` matches one id exactly; `since` keeps `timestamp >= cut` (the prior
/// Rust path's half-open lower bound, which skipped `ts < cut`).
fn activity_filter(
    session: Option<&str>,
    since: Option<i64>,
    until: Option<i64>,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    let mut clauses: Vec<&str> = Vec::new();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    if let Some(s) = session {
        params.push(Box::new(s.to_string()));
        clauses.push("session_id = ?");
    }
    if let Some(cut) = since {
        params.push(Box::new(cut));
        clauses.push("timestamp >= ?");
    }
    if let Some(cut) = until.filter(|&u| u > 0) {
        params.push(Box::new(cut));
        clauses.push("timestamp < ?");
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, params)
}

/// The machine-global `session.db` mirror for a per-repo `local` store: `home_root()`
/// joined with `session.db`, unless that equals `local` (the data dir already is home).
/// In test builds the mirror is suppressed unless `LENS_HOME` is set, so unit tests
/// never write to the developer's real `~/.lens/session.db`. Mirrors
/// [`crate::obs`]'s `global_ops_path`.
fn global_session_path(local: &Path) -> Option<PathBuf> {
    #[cfg(test)]
    {
        std::env::var_os("LENS_HOME")?;
    }
    crate::rtk::home_root()
        .map(|h| h.join("session.db"))
        .filter(|g| g != local)
}

/// Entity key for an event, or None if the category has no natural identity.
/// `file` events are keyed by their payload path; everything else is left
/// untouched (no reversible identifier in the current payloads).
fn entity_key(e: &Event) -> Option<(&str, &str)> {
    match e.category.as_str() {
        "file" => e
            .payload
            .get("path")
            .and_then(|v| v.as_str())
            .map(|p| ("file", p)),
        _ => None,
    }
}

/// Drop superseded contradictions: within each entity-key group keep only the
/// latest event by timestamp (ties keep insertion order). Keyless events and
/// the surviving keyed events are returned in their original order.
fn resolve_contradictions(events: Vec<Event>) -> Vec<Event> {
    // Winning index per key: replace on ts >= so the last-inserted at an equal
    // timestamp wins (events arrive in insertion order).
    let mut winner: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (i, e) in events.iter().enumerate() {
        if let Some((cat, key)) = entity_key(e) {
            let k = (cat.to_string(), key.to_string());
            match winner.get(&k) {
                Some(&w) if events[w].timestamp > e.timestamp => {}
                _ => {
                    winner.insert(k, i);
                }
            }
        }
    }
    let kept: BTreeSet<usize> = winner.values().copied().collect();
    events
        .into_iter()
        .enumerate()
        .filter(|(i, e)| entity_key(e).is_none() || kept.contains(i))
        .map(|(_, e)| e)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn ev(session: &str, project: &str, cat: &str, prio: u8, ts: i64) -> Event {
        Event {
            session_id: session.into(),
            project: project.into(),
            timestamp: ts,
            category: cat.into(),
            priority: prio,
            payload: json!({"k": cat}),
            source_hook: "PostToolUse".into(),
        }
    }

    #[test]
    fn insert_and_read_events() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.insert_events(&[ev("s1", "/p", "file", 1, 10), ev("s1", "/p", "git", 2, 20)])
            .unwrap();
        let got = s.events_for_session("s1").unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].category, "file");
        assert_eq!(got[1].category, "git");
    }

    #[test]
    fn clear_project_events_is_scoped() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.insert_events(&[ev("s1", "/a", "file", 1, 1), ev("s2", "/b", "file", 1, 1)])
            .unwrap();
        s.clear_project_events("/a").unwrap();
        assert_eq!(s.count_events_for_project("/a").unwrap(), 0);
        assert_eq!(s.count_events_for_project("/b").unwrap(), 1);
    }

    #[test]
    fn resume_roundtrip_and_consume() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.upsert_resume("s1", "/p", "SNAP", 3, 100).unwrap();
        let r = s.get_resume("s1", "/p").unwrap().unwrap();
        assert_eq!(r.snapshot, "SNAP");
        assert!(!r.consumed);
        s.mark_resume_consumed("s1", "/p").unwrap();
        assert!(s.get_resume("s1", "/p").unwrap().unwrap().consumed);
    }

    #[test]
    fn claim_latest_unconsumed_excludes_current() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.upsert_resume("old", "/p", "OLD", 1, 100).unwrap();
        // A fresh /resume session id; should pick up the prior snapshot.
        let got = s.claim_latest_unconsumed_resume("/p", "fresh").unwrap();
        assert_eq!(got.as_deref(), Some("OLD"));
        // Now consumed — second claim finds nothing.
        assert!(s
            .claim_latest_unconsumed_resume("/p", "fresh")
            .unwrap()
            .is_none());
    }

    #[test]
    fn activity_aggregates_by_category_and_scopes_session() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.insert_events(&[
            ev("s1", "/p", "file", 1, 10),
            ev("s1", "/p", "file", 1, 30),
            ev("s1", "/p", "error", 2, 20),
            ev("s2", "/p", "file", 1, 40),
        ])
        .unwrap();

        let all = s.activity(None, None).unwrap();
        assert_eq!(all.total_events, 4);
        assert_eq!(all.sessions, 2);
        assert_eq!(all.last_ts, Some(40));
        // file (3) ranks before error (1).
        assert_eq!(all.by_category[0], ("file".into(), 3));
        assert_eq!(all.by_category[1], ("error".into(), 1));

        let only_s1 = s.activity(Some("s1"), None).unwrap();
        assert_eq!(only_s1.total_events, 3);
        assert_eq!(only_s1.sessions, 1);

        // `since` cutoff keeps only events at/after it (the dashboard live scope):
        // ts >= 20 drops the ts=10 event, leaving 3 events across both sessions.
        let live = s.activity(None, Some(20)).unwrap();
        assert_eq!(live.total_events, 3);
        assert_eq!(live.sessions, 2);
        assert_eq!(live.last_ts, Some(40));
    }

    fn file_ev(session: &str, action: &str, path: &str, ts: i64) -> Event {
        Event {
            session_id: session.into(),
            project: "/p".into(),
            timestamp: ts,
            category: "file".into(),
            priority: 1,
            payload: json!({"action": action, "path": path}),
            source_hook: "PostToolUse".into(),
        }
    }

    #[test]
    fn resolved_events_keep_latest_per_path() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.insert_events(&[
            // Same path contradicts itself: touched at t1, deleted at t2 > t1.
            file_ev("s1", "write", "src/a.rs", 10),
            file_ev("s1", "delete", "src/a.rs", 20),
            // Unrelated path: must be unaffected.
            file_ev("s1", "edit", "src/b.rs", 15),
            // Keyless category: must pass through untouched.
            ev("s1", "/p", "decision", 2, 5),
        ])
        .unwrap();

        let got = s.resolved_events_for_session("s1").unwrap();
        // a.rs at t1 is superseded; only the t2 "delete" survives for that path.
        let a: Vec<&Event> = got
            .iter()
            .filter(|e| e.payload.get("path").and_then(|v| v.as_str()) == Some("src/a.rs"))
            .collect();
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].timestamp, 20);
        assert_eq!(a[0].payload["action"], "delete");
        // b.rs and the decision are still present.
        assert!(got
            .iter()
            .any(|e| e.payload.get("path").and_then(|v| v.as_str()) == Some("src/b.rs")));
        assert!(got.iter().any(|e| e.category == "decision"));
        // Original order preserved among survivors: decision (last inserted) stays last.
        assert_eq!(got.len(), 3);
        assert_eq!(got.last().unwrap().category, "decision");
    }

    #[test]
    fn events_mirror_into_global_home() {
        // home_root() is env-driven and process-global; serialize with other mutators.
        // While LENS_HOME is set, any concurrent SessionStore test also mirrors into
        // this home, so scope assertions to a unique session id, never a global count.
        let _g = crate::rtk::env_test_lock();
        let home = tempdir().unwrap();
        std::env::set_var("LENS_HOME", home.path());
        let data = tempdir().unwrap();
        let s = SessionStore::open(data.path()).unwrap();
        s.insert_events(&[ev("mirror-uniq", "/p", "file", 1, 10)])
            .unwrap();
        std::env::remove_var("LENS_HOME");

        // The per-repo store carries the event...
        assert_eq!(s.events_for_session("mirror-uniq").unwrap().len(), 1);
        // ...and so does the machine-global mirror under home_root().
        let global = SessionStore::open(home.path()).unwrap();
        assert_eq!(global.events_for_session("mirror-uniq").unwrap().len(), 1);
    }

    #[test]
    fn compact_count_increments() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();
        s.ensure_session("s1", "/p", 1).unwrap();
        assert_eq!(s.increment_compact_count("s1").unwrap(), 1);
        assert_eq!(s.increment_compact_count("s1").unwrap(), 2);
        assert_eq!(s.compact_count("s1").unwrap(), 2);
    }

    // ── SQL-pushdown equivalence ────────────────────────────────────────────
    //
    // The pre-pushdown `activity`/`event_buckets` filtered+grouped in Rust over a
    // full-table scan. These reference fns reproduce that exact logic; the test
    // asserts the SQL-backed methods equal them structurally on a deliberately
    // adversarial dataset (multiple sessions, categories, on-boundary timestamps,
    // an empty bucket).

    /// Verbatim copy of the prior Rust-filter `activity` aggregation.
    fn ref_activity(events: &[Event], session: Option<&str>, since: Option<i64>) -> Activity {
        let mut by_cat: BTreeMap<String, i64> = BTreeMap::new();
        let mut by_hook: BTreeMap<String, i64> = BTreeMap::new();
        let mut sessions: BTreeSet<String> = BTreeSet::new();
        let mut total = 0i64;
        let mut last_ts: Option<i64> = None;
        for e in events {
            if let Some(f) = session {
                if e.session_id != f {
                    continue;
                }
            }
            if let Some(cut) = since {
                if e.timestamp < cut {
                    continue;
                }
            }
            total += 1;
            *by_cat.entry(e.category.clone()).or_insert(0) += 1;
            *by_hook.entry(e.source_hook.clone()).or_insert(0) += 1;
            sessions.insert(e.session_id.clone());
            last_ts = Some(last_ts.map_or(e.timestamp, |p| p.max(e.timestamp)));
        }
        let sort_desc = |m: BTreeMap<String, i64>| {
            let mut v: Vec<(String, i64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v
        };
        Activity {
            total_events: total,
            sessions: sessions.len() as i64,
            last_ts,
            by_category: sort_desc(by_cat),
            by_hook: sort_desc(by_hook),
        }
    }

    /// Verbatim copy of the prior Rust-filter `event_buckets` aggregation.
    fn ref_event_buckets(
        events: &[Event],
        session: Option<&str>,
        start: i64,
        end: i64,
        n: usize,
    ) -> Vec<i64> {
        let span = (end - start).max(1);
        let mut b = vec![0i64; n];
        for e in events {
            if let Some(f) = session {
                if e.session_id != f {
                    continue;
                }
            }
            if e.timestamp < start || e.timestamp > end {
                continue;
            }
            let i = (((e.timestamp - start) as f64 / span as f64).clamp(0.0, 1.0) * n as f64)
                as usize;
            b[i.min(n - 1)] += 1;
        }
        b
    }

    fn evh(session: &str, cat: &str, hook: &str, ts: i64) -> Event {
        Event {
            session_id: session.into(),
            project: "/p".into(),
            timestamp: ts,
            category: cat.into(),
            priority: 1,
            payload: json!({"k": cat}),
            source_hook: hook.into(),
        }
    }

    #[test]
    fn sql_pushdown_matches_rust_reference() {
        let dir = tempdir().unwrap();
        let s = SessionStore::open(dir.path()).unwrap();

        // Window [100, 700], 6 buckets => each bucket spans 100s. Seed events on
        // the exact bucket edges (100,200,...,700), outside the window (50, 1000),
        // multiple categories and hooks, across three sessions. Bucket index 3
        // ([400,500)) is deliberately left empty. Edge timestamps exercise the
        // float-truncation boundary the SQL path must NOT diverge from.
        let start = 100i64;
        let end = 700i64;
        let n = 6usize;
        let events = vec![
            evh("s1", "file", "PostToolUse", 50),    // before window: excluded
            evh("s1", "file", "PostToolUse", 100),   // lower edge -> bucket 0
            evh("s1", "git", "PostToolUse", 199),    // bucket 0
            evh("s2", "file", "PreToolUse", 200),    // bucket 1
            evh("s2", "error", "PreToolUse", 250),   // bucket 1
            evh("s1", "file", "PostToolUse", 300),   // bucket 2
            evh("s3", "task", "UserPromptSubmit", 350), // bucket 2
            // (no events in [400,500) -> empty bucket 3)
            evh("s2", "file", "PreToolUse", 550),    // bucket 4
            evh("s1", "error", "PostToolUse", 600),  // bucket 5 (upper-half edge)
            evh("s3", "file", "PostToolUse", 700),   // upper edge -> clamped into last bucket
            evh("s3", "git", "PostToolUse", 1000),   // after window: excluded
            // duplicate category/session to drive count ties & distinct-session count
            evh("s2", "file", "PreToolUse", 660),    // bucket 5
            evh("s1", "git", "PostToolUse", 120),    // bucket 0
        ];
        s.insert_events(&events).unwrap();

        // activity: every scope the dashboard uses must match the reference exactly.
        for session in [None, Some("s1"), Some("s2"), Some("s3"), Some("absent")] {
            for since in [None, Some(0), Some(200), Some(250), Some(701), Some(2000)] {
                let got = s.activity(session, since).unwrap();
                let want = ref_activity(&events, session, since);
                assert_eq!(
                    got.total_events, want.total_events,
                    "total mismatch session={session:?} since={since:?}"
                );
                assert_eq!(got.sessions, want.sessions, "sessions session={session:?} since={since:?}");
                assert_eq!(got.last_ts, want.last_ts, "last_ts session={session:?} since={since:?}");
                assert_eq!(
                    got.by_category, want.by_category,
                    "by_category session={session:?} since={since:?}"
                );
                assert_eq!(
                    got.by_hook, want.by_hook,
                    "by_hook session={session:?} since={since:?}"
                );
            }
        }

        // event_buckets: full vector equality, including the empty bucket and the
        // on-edge timestamps. Exercise several windows / bucket counts.
        for session in [None, Some("s1"), Some("s2"), Some("s3"), Some("absent")] {
            for &(st, en, nn) in &[
                (start, end, n),
                (0, 1000, 10),
                (100, 700, 60),
                (200, 200, 4), // degenerate span -> clamped to 1
                (300, 350, 3),
            ] {
                let got = s.event_buckets(session, st, en, nn).unwrap();
                let want = ref_event_buckets(&events, session, st, en, nn);
                assert_eq!(
                    got, want,
                    "buckets session={session:?} start={st} end={en} n={nn}"
                );
            }
        }

        // The empty bucket is genuinely present in the canonical window.
        let canonical = s.event_buckets(None, start, end, n).unwrap();
        assert_eq!(canonical.len(), n);
        assert_eq!(canonical[3], 0, "bucket [400,500) must be empty");
        assert!(canonical[0] >= 3, "lower-edge events land in bucket 0");

        // increment_compact_count: one-statement UPDATE...RETURNING returns the
        // incremented value across repeated calls, and reads back consistently.
        s.ensure_session("c1", "/p", 1).unwrap();
        assert_eq!(s.increment_compact_count("c1").unwrap(), 1);
        assert_eq!(s.increment_compact_count("c1").unwrap(), 2);
        assert_eq!(s.increment_compact_count("c1").unwrap(), 3);
        assert_eq!(s.compact_count("c1").unwrap(), 3);
        // Unknown session: no row updated -> 0 (matches the prior unwrap_or(0)).
        assert_eq!(s.increment_compact_count("never-seen").unwrap(), 0);
    }
}
