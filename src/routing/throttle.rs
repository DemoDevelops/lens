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
/// or garbled file just starts empty). Lines are `session\tkey` (a fire,
/// incrementing the count) or `session\tkey\t!reset` (a reset sentinel,
/// zeroing the count outright) — processed in order, so a reset followed by
/// more fires counts up from zero again.
fn ensure_loaded(state: &mut DirState, data_dir: &Path) {
    if state.loaded {
        return;
    }
    state.loaded = true;
    if let Ok(text) = std::fs::read_to_string(log_path(data_dir)) {
        for line in text.lines() {
            let mut fields = line.splitn(3, '\t');
            let (Some(s), Some(k)) = (fields.next(), fields.next()) else {
                continue;
            };
            let key = (s.to_string(), k.to_string());
            if fields.next() == Some("!reset") {
                state.counts.insert(key, 0);
            } else {
                *state.counts.entry(key).or_insert(0) += 1;
            }
        }
    }
}

/// Append one fully formed line to the on-disk log (best-effort). The line is
/// emitted in ONE `write` syscall: concurrent hook processes (parallel
/// sessions share one data dir) append to this file, and `writeln!` on an
/// unbuffered `File` issues one write per format fragment, so two processes'
/// fragments interleave and tear both records — a torn `achain:done` mark /
/// `!reset` sentinel is how the once-per-session achain deny re-fired
/// (2026-07-21 parallel-lane bench audit). A single `write_all` of the whole
/// line under `O_APPEND` keeps each record intact.
fn append_line(data_dir: &Path, line: &str) {
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(data_dir))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Append one `session\tkey` fire to the on-disk log (best-effort).
fn append(data_dir: &Path, session: &str, key: &str) {
    append_line(data_dir, &format!("{session}\t{key}\n"));
}

/// Append a `session\tkey\t!reset` sentinel to the on-disk log (best-effort).
fn append_reset(data_dir: &Path, session: &str, key: &str) {
    append_line(data_dir, &format!("{session}\t{key}\t!reset\n"));
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

/// Zero the `(session, key)` fire count (cache + on-disk log, best-effort).
/// A later `bump` counts up from zero again, even in a fresh process.
pub fn reset(data_dir: &Path, session: &str, key: &str) {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    append_reset(data_dir, session, key);
    state
        .counts
        .insert((session.to_string(), key.to_string()), 0);
}

/// Cross-process atomic test-and-set for a once-per-session mark: records the
/// fire and returns true iff this call WON it; false if `(session, key)` had
/// already fired. [`fired`] + [`mark`] is check-then-act across processes:
/// each hook event is its own process whose cache loads once, so two parallel
/// tool calls in one message both pass the `fired` check and both fire (the
/// v0.10 gate log shows 30 sessions with a doubled achain deny). Locking the
/// log file and re-reading it under the lock closes that window. Best-effort
/// like the rest of the module: an IO failure falls back to the cache verdict.
pub fn try_mark(data_dir: &Path, session: &str, key: &str) -> bool {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    let ck = (session.to_string(), key.to_string());
    if state.counts.contains_key(&ck) {
        return false;
    }
    let _ = std::fs::create_dir_all(data_dir);
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(log_path(data_dir))
    {
        // Re-read under the lock: another hook process may have appended the
        // mark after this process loaded its cache. O_APPEND only pins the
        // write cursor; reads start at offset 0. Lock released on drop.
        let mut text = String::new();
        let already = f.lock().is_ok()
            && std::io::Read::read_to_string(&mut f, &mut text).is_ok()
            && text.lines().any(|l| {
                let mut it = l.splitn(3, '\t');
                it.next() == Some(session) && it.next() == Some(key)
            });
        if already {
            state.counts.insert(ck, 1);
            return false;
        }
        let _ = f.write_all(format!("{session}\t{key}\n").as_bytes());
    }
    state.counts.insert(ck, 1);
    true
}

/// Atomic check-and-consume for one-shot armed markers: if `(session, key)`
/// has a positive count, zero it and return true; otherwise leave it and
/// return false. Unlike [`fired`], a reset-to-zero key reads as disarmed.
pub fn take(data_dir: &Path, session: &str, key: &str) -> bool {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    let c = state
        .counts
        .entry((session.to_string(), key.to_string()))
        .or_insert(0);
    if *c == 0 {
        return false;
    }
    *c = 0;
    append_reset(data_dir, session, key);
    true
}

/// Non-consuming peek at a marker: true iff `(session, key)` has a positive
/// count. Unlike [`take`] it neither consumes nor writes, so it reads the same
/// value however many times it is called until a `reset`/`take` zeroes the
/// count; unlike [`fired`], a reset-to-zero key reads as disarmed. For
/// prompt-scoped latches (e.g. the rskel edit-intent exemption) that several
/// gates in one event must read without one read disarming the others.
pub fn armed(data_dir: &Path, session: &str, key: &str) -> bool {
    let mut map = throttle().0.lock().unwrap();
    let state = map.entry(data_dir.to_path_buf()).or_default();
    ensure_loaded(state, data_dir);
    state
        .counts
        .get(&(session.to_string(), key.to_string()))
        .is_some_and(|&c| c > 0)
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

    #[test]
    fn reset_zeroes_the_count() {
        let d = tempdir().unwrap();
        assert_eq!(bump(d.path(), "s", "k"), 1);
        assert_eq!(bump(d.path(), "s", "k"), 2);
        assert_eq!(bump(d.path(), "s", "k"), 3);
        reset(d.path(), "s", "k");
        assert_eq!(bump(d.path(), "s", "k"), 1);
    }

    #[test]
    fn reset_of_unknown_key_is_a_noop() {
        let d = tempdir().unwrap();
        reset(d.path(), "s", "never-bumped"); // must not panic
        assert_eq!(bump(d.path(), "s", "never-bumped"), 1);
    }

    #[test]
    fn reset_does_not_affect_other_keys() {
        let d = tempdir().unwrap();
        mark(d.path(), "s", "a");
        assert_eq!(bump(d.path(), "s", "b"), 1);
        assert_eq!(bump(d.path(), "s", "b"), 2);

        reset(d.path(), "s", "a");

        assert!(fired(d.path(), "s", "b"));
        assert_eq!(bump(d.path(), "s", "b"), 3);
    }

    #[test]
    fn reset_persists_across_processes_via_the_log() {
        // Same simulated-fresh-process trick as
        // `persists_across_processes_via_the_log`, but proving a reset (not
        // just a fire) survives the reload.
        let d = tempdir().unwrap();
        assert_eq!(bump(d.path(), "sess", "k"), 1);
        assert_eq!(bump(d.path(), "sess", "k"), 2);
        assert_eq!(bump(d.path(), "sess", "k"), 3);
        reset(d.path(), "sess", "k");

        throttle().0.lock().unwrap().remove(d.path()); // drop the cache

        assert_eq!(
            bump(d.path(), "sess", "k"),
            1,
            "a new process must honor the on-disk reset and count up from zero"
        );
    }

    #[test]
    fn concurrent_appends_never_tear_records() {
        // Parallel bench lanes (separate hook processes, one shared data dir)
        // tore log lines when a record spanned several write syscalls: a torn
        // `achain:done` / `!reset` line broke the once-per-session deny. Each
        // append goes through its own file handle here, mimicking processes;
        // every reloaded line must parse back to exactly the sessions/keys
        // written.
        let d = tempdir().unwrap();
        let dir = d.path().to_path_buf();
        // Hammer the raw append fns directly: `bump`/`mark` serialize on the
        // in-process cache mutex, but separate hook PROCESSES don't, and
        // `append` (its own file handle per call, no lock) is exactly that
        // contention shape.
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    let sess = format!("sess-{t}");
                    for i in 0..200 {
                        if i % 5 == 0 {
                            append_reset(&dir, &sess, "achain-run");
                        } else {
                            append(&dir, &sess, "achain-run");
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = std::fs::read_to_string(log_path(&dir)).unwrap();
        let mut lines = 0;
        for line in text.lines() {
            lines += 1;
            let fields: Vec<&str> = line.split('\t').collect();
            assert!(
                (fields.len() == 2 || (fields.len() == 3 && fields[2] == "!reset"))
                    && fields[0].starts_with("sess-")
                    && fields[0].len() == 6
                    && fields[1] == "achain-run",
                "torn or malformed log line: {line:?}"
            );
        }
        assert_eq!(lines, 8 * 200, "every append must land as exactly one line");
    }

    #[test]
    fn try_mark_wins_once_then_reports_fired() {
        let d = tempdir().unwrap();
        assert!(try_mark(d.path(), "s", "k:done"), "first caller wins");
        assert!(!try_mark(d.path(), "s", "k:done"), "second caller loses");
        assert!(fired(d.path(), "s", "k:done"), "the win is a recorded fire");
        assert!(try_mark(d.path(), "s2", "k:done"), "other sessions unaffected");
    }

    #[test]
    fn try_mark_sees_another_processes_mark_despite_stale_cache() {
        let d = tempdir().unwrap();
        // Load this process's cache while the log is empty.
        assert!(!fired(d.path(), "s", "k:done"));
        // Another hook process appends the mark (cache now stale).
        append(d.path(), "s", "k:done");
        assert!(
            !try_mark(d.path(), "s", "k:done"),
            "the under-lock re-read must see the other process's mark"
        );
    }

    #[test]
    fn armed_peeks_without_consuming_unlike_take() {
        let d = tempdir().unwrap();
        // Disarmed until set; a reset-to-zero key reads as disarmed (unlike
        // `fired`, which would still see the key).
        assert!(!armed(d.path(), "s", "k"));
        reset(d.path(), "s", "k");
        assert!(!armed(d.path(), "s", "k"));
        assert!(fired(d.path(), "s", "k"), "fired sees a reset-to-zero key");

        // Once set, `armed` reads true REPEATEDLY — it never consumes.
        bump(d.path(), "s", "k");
        assert!(armed(d.path(), "s", "k"));
        assert!(armed(d.path(), "s", "k"), "a second armed read must still be true");

        // `take` consumes: the first read is true, the next false — and after
        // it, `armed` reads false too.
        assert!(take(d.path(), "s", "k"));
        assert!(!take(d.path(), "s", "k"));
        assert!(!armed(d.path(), "s", "k"));
    }
}
