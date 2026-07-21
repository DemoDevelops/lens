//! Host usage reader behind the dashboard's "Actual Usage".
//!
//! Claude Code (the default host) writes one JSONL transcript per session under
//! `<config dir>/projects/**/*.jsonl`; that path is unchanged from before the
//! opencode work. opencode persists one JSON file per message under
//! `<data dir>/storage/message/<sessionID>/*.json` (data dir:
//! `$OPENCODE_DATA_DIR`, else `$XDG_DATA_HOME/opencode`, else
//! `~/.local/share/opencode`); assistant messages carry `modelID`,
//! `tokens {input, output, reasoning, cache {read, write}}`, `cost`, and
//! `time.created` (epoch ms). `read_usage` dispatches on the detected host.
//!
//! Read-only and best-effort: a missing dir or unreadable file yields an empty
//! result, never an error — this feeds a dashboard panel.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::iso8601_to_secs;

/// One model's aggregated usage over a window: turns (assistant messages),
/// token totals, and reported spend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub turns: u64,
    pub input: u64,
    pub output: u64,
    pub cache_creation: u64,
    pub cache_read: u64,
    pub cost_usd: f64,
}

/// Read and aggregate the active host's usage (Claude or opencode) into per-model
/// (or host) summary. Dispatches on LENS_HOST (default Claude). Keeps all
/// existing Claude behavior and fixtures untouched.
pub fn read_usage(
    since: Option<i64>,
    until: Option<i64>,
    cwd_filter: Option<&Path>,
) -> Vec<ModelUsage> {
    if crate::client::is_claude() {
        read_usage_claude(since, until, cwd_filter)
    } else {
        read_usage_opencode(since, until, cwd_filter)
    }
}

/// Internal: original Claude impl (renamed; behavior identical).
fn read_usage_claude(
    since: Option<i64>,
    until: Option<i64>,
    cwd_filter: Option<&Path>,
) -> Vec<ModelUsage> {
    let mut seen = HashSet::new();
    let mut agg: BTreeMap<String, ModelUsage> = BTreeMap::new();
    for config_dir in claude_config_dirs() {
        for file in find_jsonl_files(&config_dir.join("projects")) {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            for raw in content.lines() {
                let Some(line) = parse_line(raw) else {
                    continue;
                };
                if since.is_some_and(|s| line.ts < s) {
                    continue;
                }
                if until.is_some_and(|u| line.ts >= u) {
                    continue;
                }
                if cwd_filter.is_some_and(|filter| !line.cwd.starts_with(filter)) {
                    continue;
                }
                if let Some(key) = &line.dedup_key {
                    if !seen.insert(key.clone()) {
                        continue; // resumed/rewritten turn, already counted
                    }
                }
                let entry = agg.entry(line.model.clone()).or_insert_with(|| ModelUsage {
                    model: line.model,
                    turns: 0,
                    input: 0,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 0,
                    cost_usd: 0.0,
                });
                entry.turns += 1;
                entry.input += line.input;
                entry.output += line.output;
                entry.cache_creation += line.cache_creation;
                entry.cache_read += line.cache_read;
                entry.cost_usd += line.cost_usd;
            }
        }
    }
    agg.into_values().collect()
}

/// Read opencode usage from its message store:
/// `<data dir>/storage/message/<sessionID>/*.json`, one JSON file per message.
/// Only assistant messages carry usage (`modelID`, `tokens`, `cost`); user
/// messages are skipped, mirroring the Claude reader's assistant-turn counting.
/// Ignores cwd_filter (per-project scoping needs the session table; not wired).
fn read_usage_opencode(
    since: Option<i64>,
    until: Option<i64>,
    _cwd_filter: Option<&Path>,
) -> Vec<ModelUsage> {
    let mut agg: BTreeMap<String, ModelUsage> = BTreeMap::new();
    for data_dir in opencode_data_dirs() {
        let Ok(sessions) = std::fs::read_dir(data_dir.join("storage").join("message")) else {
            continue;
        };
        for session in sessions.flatten() {
            let Ok(messages) = std::fs::read_dir(session.path()) else {
                continue;
            };
            for file in messages.flatten() {
                let path = file.path();
                if path.extension().is_none_or(|e| e != "json") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let Some(m) = parse_opencode_message(&raw) else {
                    continue;
                };
                if since.is_some_and(|s| m.ts < s) || until.is_some_and(|u| m.ts >= u) {
                    continue;
                }
                let entry = agg.entry(m.model.clone()).or_insert_with(|| ModelUsage {
                    model: m.model,
                    turns: 0,
                    input: 0,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 0,
                    cost_usd: 0.0,
                });
                entry.turns += 1;
                entry.input += m.input;
                entry.output += m.output;
                entry.cache_creation += m.cache_creation;
                entry.cache_read += m.cache_read;
                entry.cost_usd += m.cost_usd;
            }
        }
    }
    agg.into_values().collect()
}

/// One assistant message's usage, extracted from an opencode message file.
struct OpencodeMessage {
    ts: i64,
    model: String,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
    cost_usd: f64,
}

/// Parse one opencode message file. `None` for user messages, malformed JSON,
/// or anything without a `tokens` object — the caller skips those.
fn parse_opencode_message(raw: &str) -> Option<OpencodeMessage> {
    let v: Value = serde_json::from_str(raw).ok()?;
    if v.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let tokens = v.get("tokens")?;
    let tok = |k: &str| tokens.get(k).and_then(Value::as_u64).unwrap_or(0);
    let cache = tokens.get("cache");
    let cache_tok =
        |k: &str| cache.and_then(|c| c.get(k)).and_then(Value::as_u64).unwrap_or(0);
    // time.created is epoch ms.
    let ts = v
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(Value::as_i64)
        .unwrap_or(0)
        / 1000;
    let model = v
        .get("modelID")
        .and_then(Value::as_str)
        .unwrap_or("opencode")
        .to_string();
    Some(OpencodeMessage {
        ts,
        model,
        input: tok("input"),
        // Reasoning tokens are billed as output; ModelUsage has no separate slot.
        output: tok("output") + tok("reasoning"),
        cache_creation: cache_tok("write"),
        cache_read: cache_tok("read"),
        cost_usd: v.get("cost").and_then(Value::as_f64).unwrap_or(0.0),
    })
}

/// Every distinct Claude Code config dir that actually holds transcripts: the
/// union of `$CLAUDE_CONFIG_DIR`, `$XDG_CONFIG_HOME/claude`, and `~/.claude`,
/// keeping only those with a `projects/` subdir (the signal it holds
/// transcripts) and deduping by canonical path so an entry pointing at the
/// default isn't globbed twice. Empty if none exist.
fn claude_config_dirs() -> Vec<PathBuf> {
    let candidates = [
        std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("claude")),
        home_dir().map(|h| h.join(".claude")),
    ];
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for dir in candidates.into_iter().flatten() {
        if !dir.join("projects").is_dir() {
            continue;
        }
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            dirs.push(dir);
        }
    }
    dirs
}

/// Every distinct opencode data dir that holds a message store: the union of
/// `$OPENCODE_DATA_DIR`, `$XDG_DATA_HOME/opencode`, and
/// `~/.local/share/opencode`, keeping only those with a `storage/message`
/// subdir (the signal it holds messages) and deduping by canonical path.
fn opencode_data_dirs() -> Vec<PathBuf> {
    let candidates = [
        std::env::var_os("OPENCODE_DATA_DIR").map(PathBuf::from),
        std::env::var_os("XDG_DATA_HOME").map(|x| PathBuf::from(x).join("opencode")),
        home_dir().map(|h| h.join(".local").join("share").join("opencode")),
    ];
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for dir in candidates.into_iter().flatten() {
        if !dir.join("storage").join("message").is_dir() {
            continue;
        }
        let key = std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if seen.insert(key) {
            dirs.push(dir);
        }
    }
    dirs
}

/// `$HOME`, else `$USERPROFILE`. `None` if neither is set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// Recursively collect every `*.jsonl` file under `root`. Best-effort: an
/// unreadable directory is skipped, not an error.
fn find_jsonl_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "jsonl") {
                out.push(path);
            }
        }
    }
    out
}

/// The fields this module cares about, extracted from one JSONL line.
struct ParsedLine {
    model: String,
    ts: i64,
    cwd: PathBuf,
    /// `(message.id, requestId)`, when both are present — the dedup key for
    /// turns Claude Code rewrites verbatim on session resume.
    dedup_key: Option<String>,
    input: u64,
    output: u64,
    cache_creation: u64,
    cache_read: u64,
    cost_usd: f64,
}

/// Parse one JSONL line into its usage fields. `None` for anything that
/// isn't a usage-bearing assistant turn (summary/system lines, malformed
/// JSON, missing fields) — the caller skips those rather than erroring.
fn parse_line(raw: &str) -> Option<ParsedLine> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(raw).ok()?;
    let message = v.get("message")?;
    let model = message.get("model")?.as_str()?.to_string();
    let ts = iso8601_to_secs(v.get("timestamp")?.as_str()?)?;
    let cwd = PathBuf::from(v.get("cwd")?.as_str()?);
    // Presence gate: a real turn line always carries this; summary/system
    // lines that happen to have model+timestamp (they shouldn't) still don't.
    v.get("isSidechain")?.as_bool()?;
    let usage = message.get("usage")?;
    let input = usage.get("input_tokens")?.as_u64()?;
    let output = usage.get("output_tokens")?.as_u64()?;
    let cache_creation = usage.get("cache_creation_input_tokens")?.as_u64()?;
    let cache_read = usage.get("cache_read_input_tokens")?.as_u64()?;
    let cost_usd = v.get("costUSD").and_then(Value::as_f64).unwrap_or(0.0);
    let dedup_key = match (
        message.get("id").and_then(Value::as_str),
        v.get("requestId").and_then(Value::as_str),
    ) {
        (Some(id), Some(rid)) => Some(format!("{id}\u{0}{rid}")),
        _ => None,
    };
    Some(ParsedLine {
        model,
        ts,
        cwd,
        dedup_key,
        input,
        output,
        cache_creation,
        cache_read,
        cost_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write the fixture into `<root>/projects/sess1/usage.jsonl` (Claude
    /// Code always nests transcripts one level under a project dir; this
    /// also exercises the recursive glob).
    fn write_fixture(root: &Path) {
        let dir = root.join("projects").join("sess1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("usage.jsonl"), include_str!("testdata/usage_sample.jsonl"))
            .unwrap();
    }

    /// Restore an env var to its prior value, or unset it if there was none.
    fn restore(key: &str, prev: Option<std::ffi::OsString>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    /// Point `CLAUDE_CONFIG_DIR` at a fresh temp dir holding the fixture for the
    /// duration of `f`, and steer `HOME`/`XDG_CONFIG_HOME` at an empty dir so
    /// the union reader can't reach the developer's real `~/.claude` (which
    /// would pollute the hand-computed sums). Restores each var after. These are
    /// process-global; serialize with `env_test_lock` like the other
    /// env-mutating tests in this crate.
    fn with_fixture<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::rtk::env_test_lock();
        let prev_cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path());
        let empty = tempfile::tempdir().unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        std::env::set_var("HOME", empty.path());
        std::env::set_var("XDG_CONFIG_HOME", empty.path());
        let out = f();
        restore("CLAUDE_CONFIG_DIR", prev_cfg);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        out
    }

    #[test]
    fn claude_config_dirs_unions_env_override_and_default() {
        let _g = crate::rtk::env_test_lock();
        let prev_cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let cfg = tempfile::tempdir().unwrap(); // $CLAUDE_CONFIG_DIR
        let home = tempfile::tempdir().unwrap(); // $HOME, holds ~/.claude
        let xdg = tempfile::tempdir().unwrap(); // $XDG_CONFIG_HOME, no claude/projects
        write_fixture(cfg.path());
        write_fixture(&home.path().join(".claude"));
        std::env::set_var("CLAUDE_CONFIG_DIR", cfg.path());
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        let dirs = claude_config_dirs();
        restore("CLAUDE_CONFIG_DIR", prev_cfg);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        // Both the custom config dir and the default ~/.claude are returned; the
        // XDG candidate has no projects/ and is dropped.
        assert_eq!(dirs.len(), 2, "got {dirs:?}");
        assert!(dirs.iter().any(|d| d == cfg.path()));
        assert!(dirs.iter().any(|d| d == &home.path().join(".claude")));
    }

    #[test]
    fn claude_config_dirs_empty_without_projects_subdir() {
        let _g = crate::rtk::env_test_lock();
        let prev_cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let empty = tempfile::tempdir().unwrap(); // no projects/ anywhere
        std::env::set_var("CLAUDE_CONFIG_DIR", empty.path());
        std::env::set_var("HOME", empty.path());
        std::env::set_var("XDG_CONFIG_HOME", empty.path());
        let dirs = claude_config_dirs();
        restore("CLAUDE_CONFIG_DIR", prev_cfg);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert!(dirs.is_empty(), "got {dirs:?}");
    }

    #[test]
    fn read_usage_spans_multiple_config_dirs() {
        let _g = crate::rtk::env_test_lock();
        let prev_cfg = std::env::var_os("CLAUDE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let cfg = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        write_fixture(cfg.path()); // opus + sonnet under $CLAUDE_CONFIG_DIR
        // The default ~/.claude, with one line for a model absent from the first
        // fixture, so its presence proves the union rather than a lucky overlap.
        let d = home.path().join(".claude").join("projects").join("s");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("u.jsonl"),
            r#"{"timestamp":"2024-06-01T00:00:00Z","cwd":"/Users/dev/projA","isSidechain":false,"requestId":"rZ","message":{"id":"mZ","model":"other-account-model","usage":{"input_tokens":7,"output_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        )
        .unwrap();
        std::env::set_var("CLAUDE_CONFIG_DIR", cfg.path());
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        let usage = read_usage(None, None, None);
        restore("CLAUDE_CONFIG_DIR", prev_cfg);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert!(usage.iter().any(|m| m.model == "claude-opus-4-8"));
        let other = usage
            .iter()
            .find(|m| m.model == "other-account-model")
            .expect("the default ~/.claude's usage must be included");
        assert_eq!(other.turns, 1);
        assert_eq!(other.input, 7);
    }

    #[test]
    fn duplicate_message_id_and_request_id_counted_once() {
        let usage = with_fixture(|| read_usage(None, None, None));
        let opus = usage.iter().find(|m| m.model == "claude-opus-4-8").unwrap();
        // Fixture has 5 opus lines with usage (early, dup-pair x2, sidechain,
        // late); the duplicate must collapse to one, leaving 4 turns.
        assert_eq!(opus.turns, 4);
        assert_eq!(opus.input, 1000 + 100 + 200 + 5000);
        assert_eq!(opus.output, 1000 + 50 + 80 + 5000);
    }

    #[test]
    fn per_model_sums_match_hand_computed_values() {
        let usage = with_fixture(|| read_usage(None, None, None));

        let opus = usage.iter().find(|m| m.model == "claude-opus-4-8").unwrap();
        assert_eq!(opus.turns, 4);
        assert_eq!(opus.input, 6300);
        assert_eq!(opus.output, 6130);
        assert_eq!(opus.cache_creation, 10);
        assert_eq!(opus.cache_read, 25);
        assert!((opus.cost_usd - 0.03).abs() < 1e-9);

        let sonnet = usage
            .iter()
            .find(|m| m.model == "claude-sonnet-5")
            .unwrap();
        assert_eq!(sonnet.turns, 1);
        assert_eq!(sonnet.input, 300);
        assert_eq!(sonnet.output, 150);
        assert_eq!(sonnet.cache_creation, 5);
        assert_eq!(sonnet.cache_read, 0);
        assert!((sonnet.cost_usd - 0.05).abs() < 1e-9);
    }

    #[test]
    fn cwd_filter_excludes_off_path_line() {
        let usage =
            with_fixture(|| read_usage(None, None, Some(Path::new("/Users/dev/projA"))));
        assert!(
            usage.iter().all(|m| m.model != "claude-sonnet-5"),
            "sonnet's only line is under /Users/dev/other, must be excluded"
        );
        let opus = usage.iter().find(|m| m.model == "claude-opus-4-8").unwrap();
        assert_eq!(opus.turns, 4, "every opus line is under /Users/dev/projA");
    }

    #[test]
    fn window_since_until_bounds_are_honored() {
        let since = iso8601_to_secs("2024-03-01T00:00:00Z");
        let until = iso8601_to_secs("2024-09-01T00:00:00Z");
        let usage = with_fixture(|| read_usage(since, until, None));

        // Only the two mid-window opus lines survive: the early (Jan) and
        // late (Dec) lines fall outside [since, until). One of the two mid
        // lines is isSidechain:true, so this also proves sidechain turns are
        // counted, not skipped: input would be 100 (not 300) if it weren't.
        let opus = usage.iter().find(|m| m.model == "claude-opus-4-8").unwrap();
        assert_eq!(opus.turns, 2);
        assert_eq!(opus.input, 300);
        assert_eq!(opus.output, 130);

        let sonnet = usage
            .iter()
            .find(|m| m.model == "claude-sonnet-5")
            .unwrap();
        assert_eq!(sonnet.turns, 1);
    }

    #[test]
    fn malformed_lines_are_skipped_without_panicking() {
        let usage = with_fixture(|| read_usage(None, None, None));
        let total_turns: u64 = usage.iter().map(|m| m.turns).sum();
        // 4 opus + 1 sonnet; the summary line, the line missing `timestamp`,
        // and the duplicate are all excluded.
        assert_eq!(total_turns, 5);
    }

    /// Write an opencode message-store fixture into `<root>/storage/message/`:
    /// three assistant messages (two grok-4, one grok-3-mini across two
    /// sessions) plus one user message that must be skipped.
    fn write_opencode_storage_fixture(root: &Path) {
        let s1 = root.join("storage").join("message").join("ses_a");
        let s2 = root.join("storage").join("message").join("ses_b");
        std::fs::create_dir_all(&s1).unwrap();
        std::fs::create_dir_all(&s2).unwrap();
        std::fs::write(
            s1.join("msg_1.json"),
            r#"{"id":"msg_1","role":"user","sessionID":"ses_a","time":{"created":1752192000000}}"#,
        )
        .unwrap();
        std::fs::write(
            s1.join("msg_2.json"),
            r#"{"id":"msg_2","role":"assistant","sessionID":"ses_a","modelID":"grok-4","providerID":"xai","cost":0.01,"tokens":{"input":100,"output":50,"reasoning":10,"cache":{"read":30,"write":20}},"time":{"created":1752192060000}}"#,
        )
        .unwrap();
        std::fs::write(
            s1.join("msg_3.json"),
            r#"{"id":"msg_3","role":"assistant","sessionID":"ses_a","modelID":"grok-4","providerID":"xai","cost":0.02,"tokens":{"input":200,"output":80,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1752192120000}}"#,
        )
        .unwrap();
        std::fs::write(
            s2.join("msg_4.json"),
            r#"{"id":"msg_4","role":"assistant","sessionID":"ses_b","modelID":"grok-3-mini","providerID":"xai","cost":0.0,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},"time":{"created":1752195600000}}"#,
        )
        .unwrap();
    }

    /// Point LENS_HOST=opencode + OPENCODE_DATA_DIR at a temp message store.
    /// Steer HOME/XDG_DATA_HOME empty. Restores. Serialize via env_test_lock.
    fn with_opencode_fixture<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_data = std::env::var_os("OPENCODE_DATA_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        let tmp = tempfile::tempdir().unwrap();
        write_opencode_storage_fixture(tmp.path());
        let empty = tempfile::tempdir().unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_DATA_DIR", tmp.path());
        std::env::set_var("HOME", empty.path());
        std::env::set_var("XDG_DATA_HOME", empty.path());
        let out = f();
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_DATA_DIR", prev_data);
        restore("HOME", prev_home);
        restore("XDG_DATA_HOME", prev_xdg);
        out
    }

    #[test]
    fn opencode_data_dirs_finds_env_dir_and_skips_storeless() {
        with_opencode_fixture(|| {
            let dirs = opencode_data_dirs();
            // Only the env dir qualifies: HOME/XDG_DATA_HOME point at empty
            // tempdirs with no storage/message.
            assert_eq!(dirs.len(), 1, "got {dirs:?}");
        });
    }

    #[test]
    fn read_usage_opencode_aggregates_assistant_messages_per_model() {
        let usage = with_opencode_fixture(|| read_usage(None, None, None));
        assert_eq!(usage.len(), 2, "one row per model, no phantom rows: {usage:?}");
        let grok4 = usage.iter().find(|m| m.model == "grok-4").unwrap();
        assert_eq!(grok4.turns, 2, "user messages are not turns");
        assert_eq!(grok4.input, 300);
        assert_eq!(grok4.output, 130 + 10, "reasoning folds into output");
        assert_eq!(grok4.cache_read, 30);
        assert_eq!(grok4.cache_creation, 20);
        assert!((grok4.cost_usd - 0.03).abs() < 1e-9);
        let mini = usage.iter().find(|m| m.model == "grok-3-mini").unwrap();
        assert_eq!(mini.turns, 1);
        assert_eq!(mini.input, 10);
    }

    #[test]
    fn read_usage_opencode_respects_window() {
        // Fixture timestamps: msg_2 at 1752192060, msg_3 at 1752192120,
        // msg_4 at 1752195600 (all epoch seconds).
        let usage =
            with_opencode_fixture(|| read_usage(Some(1752192100), Some(1752195600), None));
        assert_eq!(usage.len(), 1, "only msg_3 falls in the window: {usage:?}");
        assert_eq!(usage[0].model, "grok-4");
        assert_eq!(usage[0].turns, 1);
        assert_eq!(usage[0].input, 200);
    }

    #[test]
    fn opencode_message_without_model_id_falls_back_to_host_label() {
        assert_eq!(
            parse_opencode_message(
                r#"{"role":"assistant","tokens":{"input":1,"output":1},"time":{"created":1000}}"#
            )
            .unwrap()
            .model,
            "opencode"
        );
        // user messages and token-less messages are skipped
        assert!(parse_opencode_message(r#"{"role":"user","time":{"created":1000}}"#).is_none());
        assert!(parse_opencode_message(r#"{"role":"assistant"}"#).is_none());
        assert!(parse_opencode_message("not json").is_none());
    }
}
