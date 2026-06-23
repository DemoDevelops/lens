//! Nudge throttle keyed by `(session, key)`, persisted per data dir so it
//! survives across the per-event hook processes (each `lens hook` invocation is
//! its own process, so a pure in-memory map would re-fire every nudge).
//!
//! Source of truth: an append-only log `<data_dir>/routing_nudges.tsv` (one
//! `session\tkey` line per fire). A per-process in-memory cache, loaded once per
//! data dir, keeps the hot `fired` check allocation-light. No TTL: a key fires
//! once per session, then never again. The tally doubles as the counter for the
//! periodic nudges (read-graph escalation, external-MCP), so they persist here
//! too instead of as per-(session,key) marker files under `throttle/`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// One data dir's fire counts, loaded lazily from its on-disk log.
#[derive(Default)]
struct DirState {
    counts: HashMap<(String, String), u64>,
    loaded: bool,
}

struct NudgeThrottle(Mutex<HashMap<PathBuf, DirState>>);

static THROTTLE: OnceLock<NudgeThrottle> = OnceLock::new();

fn throttle() -> &'static NudgeThrottle {
    THROTTLE.get_or_init(|| NudgeThrottle(Mutex::new(HashMap::new())))
}

fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("routing_nudges.tsv")
}

/// Load a data dir's log into `state` once per process (best-effort; a missing
/// or garbled file just starts empty).
fn ensure_loaded(state: &mut DirState, data_dir: &Path) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    if let Ok(text) = std::fs::read_to_string(log_path(data_dir)) {
        for line in text.lines() {
            if let Some((s, k)) = line.split_once('\t') {
                *state
                    .counts
                    .entry((s.to_string(), k.to_string()))
                    .or_insert(0) += 1;
            }
        }
    }
}

/// Append one `session\tkey` fire to the on-disk log (best-effort).
fn append(data_dir: &Path, session: &str, key: &str) {
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(data_dir))
    {
        let _ = writeln!(f, "{session}\t{key}");
    }
}

/// Has `(session, key)` already fired this session? (cross-process)
pub fn fired(data_dir: &Path, session: &str, key: &str) -> bool {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    state
        .counts
        .contains_key(&(session.to_string(), key.to_string()))
}

/// Record that `(session, key)` fired (cache + on-disk log). Idempotent.
pub fn mark(data_dir: &Path, session: &str, key: &str) {
    use std::collections::hash_map::Entry;
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    if let Entry::Vacant(e) = state.counts.entry((session.to_string(), key.to_string())) {
        append(data_dir, session, key);
        e.insert(1);
    }
}

/// Increment and return the `(session, key)` fire count (first call returns 1).
pub fn bump(data_dir: &Path, session: &str, key: &str) -> u64 {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    append(data_dir, session, key);
    let c = state
        .counts
        .entry((session.to_string(), key.to_string()))
        .or_insert(0);
    *c += 1;
    *c
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn fired_is_false_until_marked() {
        let d = tempdir().unwrap();
        assert!(!fired(d.path(), "s", "k"));
        mark(d.path(), "s", "k");
        assert!(fired(d.path(), "s", "k"));
        assert!(!fired(d.path(), "s", "other"));
    }

    #[test]
    fn bump_counts_per_key() {
        let d = tempdir().unwrap();
        assert_eq!(bump(d.path(), "s", "k"), 1);
        assert_eq!(bump(d.path(), "s", "k"), 2);
        assert_eq!(bump(d.path(), "s", "k"), 3);
        assert_eq!(bump(d.path(), "s", "other"), 1);
    }

    #[test]
    fn persists_across_processes_via_the_log() {
        // A fresh process is a fresh cache: simulate by clearing the in-memory
        // state for this data dir, then re-reading from the on-disk log.
        let d = tempdir().unwrap();
        mark(d.path(), "sess", "grep");
        throttle().0.lock().unwrap().remove(d.path()); // drop the cache
        assert!(
            fired(d.path(), "sess", "grep"),
            "a new process must see the prior fire from the log"
        );
    }
}
