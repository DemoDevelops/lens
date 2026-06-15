//! Session continuity: capture lifecycle events, build a priority-tiered
//! resume snapshot at compaction, and re-inject a Session Guide on resume.
//!
//! This is the active counterpart to the passive MCP tool server. It is driven
//! by the `ctxforge hook <platform> <event>` subcommand (see [`hook`]), which
//! Claude Code invokes on PreToolUse / PostToolUse / UserPromptSubmit /
//! PreCompact / SessionStart. [`install`] registers/removes those hooks.

pub mod extract;
pub mod hook;
pub mod install;
pub mod snapshot;
pub mod store;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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

/// Resolve the ctxforge data dir for a given project, matching the server's
/// convention: `$CTXFORGE_DIR` if set, else `<project>/.ctxforge`.
pub fn resolve_data_dir(project: &Path) -> PathBuf {
    match std::env::var_os("CTXFORGE_DIR") {
        Some(d) => PathBuf::from(d),
        None => project.join(".ctxforge"),
    }
}

/// Snapshot byte budget: `$CTXFORGE_SNAPSHOT_BUDGET` or 2048.
pub fn snapshot_budget() -> usize {
    std::env::var("CTXFORGE_SNAPSHOT_BUDGET")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2048)
}
