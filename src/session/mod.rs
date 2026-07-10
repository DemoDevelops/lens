//! Session continuity: capture lifecycle events, build a priority-tiered
//! resume snapshot at compaction, and re-inject a Session Guide on resume.
//!
//! This is the active counterpart to the passive MCP tool server. It is driven
//! by the `lens hook <platform> <event>` subcommand (see [`hook`]), which
//! Claude Code invokes on PreToolUse / PostToolUse / UserPromptSubmit /
//! PreCompact / SessionStart. [`install`] registers/removes those hooks.

pub mod extract;
pub mod hook;
pub mod install;
pub mod snapshot;
pub mod store;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::index::Index;
use store::SessionStore;

/// One captured lifecycle event. `priority` is 1 (critical) .. 4 (low); see the
/// taxonomy in the continuity plan. `payload` is category-specific JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub session_id: String,
    pub project: String,
    pub timestamp: i64,
    pub category: String,
    pub priority: u8,
    pub payload: serde_json::Value,
    pub source_hook: String,
}

/// An event produced by an extractor before it is attributed to a session
/// (no `session_id` / `project` / `timestamp` yet).
#[derive(Debug, Clone, PartialEq)]
pub struct RawEvent {
    pub category: String,
    pub priority: u8,
    pub payload: serde_json::Value,
}

impl RawEvent {
    pub fn new(category: &str, priority: u8, payload: serde_json::Value) -> Self {
        RawEvent {
            category: category.to_string(),
            priority,
            payload,
        }
    }

    /// Attribute this raw event to a session at a point in time.
    pub fn attribute(self, session_id: &str, project: &str, ts: i64, source_hook: &str) -> Event {
        Event {
            session_id: session_id.to_string(),
            project: project.to_string(),
            timestamp: ts,
            category: self.category,
            priority: self.priority,
            payload: self.payload,
            source_hook: source_hook.to_string(),
        }
    }
}

/// Current unix time in seconds. Hooks stamp events with this; tests pass fixed
/// timestamps directly so snapshots are deterministic.
pub fn now_ts() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Resolve the lens data dir for a given project, matching the server's
/// convention: `$LENS_DIR` if set, else `<project>/.lens`.
pub fn resolve_data_dir(project: &Path) -> PathBuf {
    match std::env::var_os("LENS_DIR") {
        Some(d) => PathBuf::from(d),
        None => project.join(".lens"),
    }
}

/// Snapshot byte budget: `$LENS_SNAPSHOT_BUDGET` or 8192. The larger default lets
/// the resume snapshot retain more of a long session's lower-priority context
/// (git ops, environment, refs) that a 2048 budget dropped, raising recovery
/// recall while staying small enough to re-inject cheaply at resume.
pub fn snapshot_budget() -> usize {
    std::env::var("LENS_SNAPSHOT_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(8192)
}

/// Record one durable project-memory item: validates `category` against
/// [`store::MEMORY_CATEGORIES`], writes it as an `Event` via
/// `SessionStore::insert_events` (which mirrors durable categories into
/// `project_memory`), and mirrors the same text into the FTS index under
/// `session://memory/<category>` so `lens_search` finds it too (best-effort,
/// mirroring [`hook::index_events`]'s tolerance for a failed write).
/// Shared by the `lens_memory_record` tool handler and the accuracy/gate
/// harness, so both go through the exact same write path.
pub fn record_memory(
    store: &SessionStore,
    index: &Index,
    project: &str,
    category: &str,
    text: &str,
) -> Result<()> {
    if !store::MEMORY_CATEGORIES.contains(&category) {
        bail!(
            "invalid memory category '{category}': must be one of {}",
            store::MEMORY_CATEGORIES.join(", ")
        );
    }
    let ts = now_ts();
    // Rule events carry their durable text under `path` (matching the file-path
    // rules `hook::capture_rules` records); every other durable category carries
    // it under `text`, matching `store::memory_item`'s extraction per category.
    let payload = if category == "rule" {
        serde_json::json!({"path": text})
    } else {
        serde_json::json!({"text": text})
    };
    let event = Event {
        session_id: format!("mcp-{ts}"),
        project: project.to_string(),
        timestamp: ts,
        category: category.to_string(),
        // Matches the hooks' own tiers for these categories: rules are P1
        // (`capture_rules`), decisions/constraints are P2 (`extract`);
        // rejected-approach has no hook precedent and takes its siblings' tier.
        priority: if category == "rule" { 1 } else { 2 },
        payload,
        source_hook: "mcp".to_string(),
    };
    store.insert_events(&[event])?;
    let path = format!("session://memory/{category}");
    let chunk_id = format!("{path}#{ts}");
    let content = format!("[{category}] {text}");
    let _ = index.index_records(&[(path, chunk_id, content)]);
    Ok(())
}

/// Durable project memory for `project`, optionally ranked by case-insensitive
/// token overlap between `query` and `(category, text)`, truncated to `limit`.
/// With no query, returns `SessionStore::project_memory`'s own oldest-first
/// (i.e. newest-last) order untouched. Deterministic: no model call, no new
/// dependency.
pub fn query_memory(
    store: &SessionStore,
    project: &str,
    query: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    let items = store.project_memory(project)?;
    Ok(match query {
        Some(q) if !q.trim().is_empty() => rank_by_overlap(items, q, limit),
        _ => items,
    })
}

/// Sort `items` by descending case-insensitive whitespace-token overlap with
/// `query` (stable, so equal-scored items keep `items`' original order), then
/// truncate to `limit`.
fn rank_by_overlap(
    items: Vec<(String, String)>,
    query: &str,
    limit: usize,
) -> Vec<(String, String)> {
    let q_tokens: BTreeSet<String> = query
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect();
    let mut scored: Vec<(usize, (String, String))> = items
        .into_iter()
        .map(|(category, text)| {
            let hay = format!("{category} {text}").to_lowercase();
            let hay_tokens: BTreeSet<&str> = hay.split_whitespace().collect();
            let overlap = q_tokens.iter().filter(|t| hay_tokens.contains(t.as_str())).count();
            (overlap, (category, text))
        })
        .collect();
    scored.sort_by_key(|(overlap, _)| std::cmp::Reverse(*overlap));
    scored.into_iter().take(limit).map(|(_, item)| item).collect()
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use tempfile::tempdir;

    fn stores(dir: &Path) -> (SessionStore, Index) {
        (
            SessionStore::open(dir).unwrap(),
            Index::open(dir).unwrap(),
        )
    }

    #[test]
    fn invalid_category_lists_the_valid_set() {
        let dir = tempdir().unwrap();
        let (store, index) = stores(dir.path());
        let err = record_memory(&store, &index, "/p", "todo", "text").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid memory category"));
        for cat in store::MEMORY_CATEGORIES {
            assert!(msg.contains(cat), "error must list '{cat}': {msg}");
        }
    }

    #[test]
    fn record_then_query_roundtrips_and_is_searchable() {
        let dir = tempdir().unwrap();
        let (store, index) = stores(dir.path());
        record_memory(&store, &index, "/p", "decision", "use RRF fusion").unwrap();
        record_memory(&store, &index, "/p", "rule", "CLAUDE.md").unwrap();

        let all = query_memory(&store, "/p", None, 20).unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.contains(&("decision".to_string(), "use RRF fusion".to_string())));
        assert!(all.contains(&("rule".to_string(), "CLAUDE.md".to_string())));

        // The FTS mirror is searchable under session://memory/<category>.
        let hits = index.search(&["RRF fusion".to_string()], 5).unwrap();
        assert!(hits.results[0]
            .hits
            .iter()
            .any(|h| h.path == "session://memory/decision"));
    }

    #[test]
    fn query_ranks_by_token_overlap_and_truncates() {
        let dir = tempdir().unwrap();
        let (store, index) = stores(dir.path());
        record_memory(&store, &index, "/p", "decision", "adopt RRF fusion ranking").unwrap();
        record_memory(&store, &index, "/p", "constraint", "never vendor ast-grep").unwrap();

        let ranked = query_memory(&store, "/p", Some("RRF fusion"), 1).unwrap();
        assert_eq!(ranked.len(), 1, "limit truncates to 1");
        assert_eq!(ranked[0].0, "decision", "the RRF-relevant item ranks first");
    }
}
