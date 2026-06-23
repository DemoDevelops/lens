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
        for e in events {
            tx.execute(
                "INSERT INTO session_events
                   (session_id, project, timestamp, category, priority, payload, source_hook)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    e.session_id,
                    e.project,
                    e.timestamp,
                    e.category,
                    e.priority,
                    e.payload.to_string(),
                    e.source_hook,
                ],
            )?;
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
        conn.execute(
            "UPDATE session_meta SET compact_count = compact_count + 1 WHERE session_id = ?1",
            [session_id],
        )?;
        let n: i64 = conn
            .query_row(
                "SELECT compact_count FROM session_meta WHERE session_id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        Ok(n)
    }

    /// Aggregate hook-captured activity, optionally scoped to one session.
    /// Read-only: aggregates rows in Rust so the (bounded, per-session) event
    /// set needs no extra indexes or grouped queries.
    pub fn activity(&self, session: Option<&str>, since: Option<i64>) -> Result<Activity> {
        let conn = self.conn()?;
        let mut stmt = conn
            .prepare("SELECT session_id, category, source_hook, timestamp FROM session_events")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;

        let mut by_cat: BTreeMap<String, i64> = BTreeMap::new();
        let mut by_hook: BTreeMap<String, i64> = BTreeMap::new();
        let mut sessions: BTreeSet<String> = BTreeSet::new();
        let mut total = 0i64;
        let mut last_ts: Option<i64> = None;
        for (sid, cat, hook, ts) in rows.flatten() {
            if let Some(f) = session {
                if sid != f {
                    continue;
                }
            }
            // `since` cutoff: keep only events at/after it (the dashboard's
            // "live since the page loaded" scope).
            if let Some(cut) = since {
                if ts < cut {
                    continue;
                }
            }
            total += 1;
            *by_cat.entry(cat).or_insert(0) += 1;
            *by_hook.entry(hook).or_insert(0) += 1;
            sessions.insert(sid);
            last_ts = Some(last_ts.map_or(ts, |p| p.max(ts)));
        }

        let sort_desc = |m: BTreeMap<String, i64>| {
            let mut v: Vec<(String, i64)> = m.into_iter().collect();
            v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            v
        };
        Ok(Activity {
            total_events: total,
            sessions: sessions.len() as i64,
            last_ts,
            by_category: sort_desc(by_cat),
            by_hook: sort_desc(by_hook),
        })
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
        let mut stmt = conn.prepare("SELECT session_id, timestamp FROM session_events")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let span = (end - start).max(1);
        let mut b = vec![0i64; n];
        for (sid, ts) in rows.flatten() {
            if let Some(f) = session {
                if sid != f {
                    continue;
                }
            }
            if ts < start || ts > end {
                continue;
            }
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
}
