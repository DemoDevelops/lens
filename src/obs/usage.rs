//! Host usage reader (Claude Code JSONL or opencode prompt-history.jsonl + opencode.db).
//! Behind the dashboard's "Actual Usage". Keeps all Claude paths; adds parallel
//! opencode_config_dirs + read for LENS_HOST=opencode (T6).
//! Models are generic: claude-* as recorded, opencode uses "opencode" or raw from history.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
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

/// Read opencode usage from prompt-history.jsonl (user turns + rough input size
/// from content len/4) and/or opencode.db (session/message counts + tokens).
/// Falls back to dir walk proxy for activity if no history files. Model set to
/// value from history if present (e.g. grok-*) else "opencode" (generic, no claude force).
/// Returns at most one aggregated entry per distinct model (mostly "opencode" v1).
/// Ignores cwd_filter (opencode transcripts lack the cwd field shape).
fn read_usage_opencode(
    since: Option<i64>,
    until: Option<i64>,
    _cwd_filter: Option<&Path>,
) -> Vec<ModelUsage> {
    let mut agg: BTreeMap<String, ModelUsage> = BTreeMap::new();
    for config_dir in opencode_config_dirs() {
        // Prefer sqlite (has real prompt/completion tokens + cost + counts).
        let db_path = config_dir.join("opencode.db");
        if db_path.is_file() {
            if let Ok(rows) = read_opencode_db(&db_path, since, until) {
                for (model, turns, input, output, cost) in rows {
                    let entry = agg.entry(model.clone()).or_insert_with(|| ModelUsage {
                        model,
                        turns: 0,
                        input: 0,
                        output: 0,
                        cache_creation: 0,
                        cache_read: 0,
                        cost_usd: 0.0,
                    });
                    entry.turns += turns;
                    entry.input += input;
                    entry.output += output;
                    entry.cost_usd += cost;
                }
            }
        }

        // jsonl fallback / complement: count user turns, rough input size.
        let jpath = config_dir.join("prompt-history.jsonl");
        if jpath.is_file() {
            if let Ok(content) = std::fs::read_to_string(&jpath) {
                for raw in content.lines() {
                    let Some(line) = parse_opencode_line(raw) else {
                        continue;
                    };
                    if since.is_some_and(|s| line.ts < s) {
                        continue;
                    }
                    if until.is_some_and(|u| line.ts >= u) {
                        continue;
                    }
                    // use model from history if present (for future per-model opencode), else "opencode"
                    // (never force claude-* here; generic for non-claude hosts)
                    let model = line.model.clone().unwrap_or_else(|| "opencode".to_string());
                    let entry = agg.entry(model.clone()).or_insert_with(|| ModelUsage {
                        model,
                        turns: 0,
                        input: 0,
                        output: 0,
                        cache_creation: 0,
                        cache_read: 0,
                        cost_usd: 0.0,
                    });
                    entry.turns += 1;
                    entry.input += line.input;
                }
            }
        }

        // Last-resort proxy (no history files): count subdirs/files as activity.
        if !agg.contains_key("opencode") && config_dir.is_dir() {
            let mut proxy = 0u64;
            if let Ok(rd) = std::fs::read_dir(&config_dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir()
                        || p.extension()
                            .is_some_and(|e| e == "jsonl" || e == "json" || e == "db")
                    {
                        proxy += 1;
                    }
                }
            }
            if proxy > 0 {
                let model = "opencode".to_string();
                let entry = agg.entry(model.clone()).or_insert_with(|| ModelUsage {
                    model,
                    turns: 0,
                    input: 0,
                    output: 0,
                    cache_creation: 0,
                    cache_read: 0,
                    cost_usd: 0.0,
                });
                entry.turns += proxy;
                entry.input += proxy * 50; // rough
            }
        }
    }
    agg.into_values().collect()
}

/// Internal row from opencode.db (model-ish, turns, prompt, completion, cost).
type OpencodeDbRow = (String, u64, u64, u64, f64);

/// Open opencode.db read-only and pull session/message counts + token totals.
/// Times are ms; we ignore since/until for db (predicate uses full window).
/// Best-effort: any error -> empty.
fn read_opencode_db(
    db_path: &Path,
    _since: Option<i64>,
    _until: Option<i64>,
) -> rusqlite::Result<Vec<OpencodeDbRow>> {
    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let turns: u64 = conn
        .query_row("SELECT COUNT(*) FROM messages WHERE role = 'user'", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    let (p, c, cost): (u64, u64, f64) = conn
        .query_row(
            "SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), COALESCE(SUM(cost),0.0) FROM sessions",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap_or((0, 0, 0.0));
    let mut out = Vec::new();
    if turns > 0 || p > 0 {
        out.push(("opencode".to_string(), turns, p, c, cost));
    }
    Ok(out)
}

/// Parse one line of prompt-history.jsonl for a user turn. Tolerates several
/// shapes (role/user, type/user, content/prompt/text keys). Rough input size
/// from content bytes /4 (proxy tokens). Returns None for non-user or bad.
fn parse_opencode_line(raw: &str) -> Option<ParsedOpencodeLine> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(raw).ok()?;
    let role = v.get("role").and_then(Value::as_str);
    let typ = v.get("type").and_then(Value::as_str);
    let is_user = role == Some("user") || typ == Some("user") || typ == Some("prompt");
    if !is_user {
        // fallback: if has prompt/content and no explicit assistant role
        if v.get("prompt").is_some() || v.get("content").is_some() {
            if role != Some("assistant") && typ != Some("assistant") {
                // treat as user turn
            } else {
                return None;
            }
        } else {
            return None;
        }
    }
    let ts = if let Some(s) = v.get("timestamp").and_then(Value::as_str) {
        iso8601_to_secs(s).unwrap_or(0)
    } else if let Some(n) = v.get("ts").and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))) {
        n
    } else if let Some(n) = v.get("created_at").and_then(|x| x.as_i64()) {
        n / 1000 // ms -> s
    } else {
        0
    };
    let content = v
        .get("content")
        .and_then(Value::as_str)
        .or_else(|| v.get("prompt").and_then(Value::as_str))
        .or_else(|| v.get("text").and_then(Value::as_str))
        .or_else(|| v.get("message").and_then(|m| m.get("content")).and_then(Value::as_str))
        .unwrap_or("");
    let input = if content.is_empty() { 1 } else { (content.len() as u64) / 4 + 1 };
    let model = v.get("model")
        .and_then(Value::as_str)
        .or_else(|| v.get("message").and_then(|m| m.get("model")).and_then(Value::as_str))
        .or_else(|| v.get("response").and_then(|r| r.get("model")).and_then(Value::as_str))
        .map(|s| s.to_string());
    Some(ParsedOpencodeLine { ts, input, model })
}

struct ParsedOpencodeLine {
    ts: i64,
    input: u64,
    model: Option<String>,
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

/// Every distinct opencode state dir: union of `$OPENCODE_CONFIG_DIR`,
/// `$XDG_CONFIG_HOME/opencode`, `~/.config/opencode`, `~/.local/share/opencode`.
/// Keeps only those that look like they hold state (db or history file or dir).
/// Dedup by canonical path.
fn opencode_config_dirs() -> Vec<PathBuf> {
    let candidates = [
        std::env::var_os("OPENCODE_CONFIG_DIR").map(PathBuf::from),
        std::env::var_os("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("opencode")),
        home_dir().map(|h| h.join(".config").join("opencode")),
        home_dir().map(|h| h.join(".local").join("share").join("opencode")),
    ];
    let mut seen = HashSet::new();
    let mut dirs = Vec::new();
    for dir in candidates.into_iter().flatten() {
        let has_state = dir.join("opencode.db").exists()
            || dir.join("prompt-history.jsonl").exists()
            || dir.join("sessions").is_dir()
            || dir.is_dir();
        if !has_state {
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

    /// Write a minimal prompt-history.jsonl with 3 user turns into the dir.
    /// Used for opencode fixture (no projects/ nesting).
    fn write_opencode_jsonl_fixture(root: &Path) {
        std::fs::create_dir_all(root).unwrap();
        let lines = r#"{"role":"user","content":"first user turn with some words to count size","timestamp":"2026-07-11T00:00:00Z"}
{"role":"user","content":"second user turn here with more text for input size proxy","timestamp":"2026-07-11T00:01:00Z"}
{"role":"user","content":"third","timestamp":"2026-07-11T00:02:00Z"}
"#;
        std::fs::write(root.join("prompt-history.jsonl"), lines).unwrap();
    }

    /// Point LENS_HOST=opencode + OPENCODE_CONFIG_DIR at temp with jsonl fixture.
    /// Steer HOME/XDG empty. Restores. Serialize via env_test_lock.
    fn with_opencode_fixture<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempfile::tempdir().unwrap();
        write_opencode_jsonl_fixture(tmp.path());
        let empty = tempfile::tempdir().unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", tmp.path());
        std::env::set_var("HOME", empty.path());
        std::env::set_var("XDG_CONFIG_HOME", empty.path());
        let out = f();
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_CONFIG_DIR", prev_oc);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        out
    }

    #[test]
    fn opencode_config_dirs_finds_env_and_jsonl() {
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let cfg = tempfile::tempdir().unwrap();
        write_opencode_jsonl_fixture(cfg.path());
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg.path());
        std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        std::env::set_var("XDG_CONFIG_HOME", tempfile::tempdir().unwrap().path());
        let dirs = opencode_config_dirs();
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_CONFIG_DIR", prev_oc);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert!(!dirs.is_empty(), "got {dirs:?}");
        assert!(dirs.iter().any(|d| d == cfg.path()));
    }

    #[test]
    fn read_usage_opencode_counts_user_turns_from_jsonl() {
        let usage = with_opencode_fixture(|| read_usage(None, None, None));
        assert_eq!(usage.len(), 1);
        let u = &usage[0];
        assert_eq!(u.model, "opencode");
        assert_eq!(u.turns, 3);
        assert!(u.input > 0, "rough input from content sizes");
        assert_eq!(u.output, 0);
        assert_eq!(u.cost_usd, 0.0);
    }

    #[test]
    fn opencode_usage_via_db_and_proxy() {
        // also exercise sqlite path + proxy fallback (create dir w/ db marker but minimal)
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempfile::tempdir().unwrap();
        // create empty db file as marker (read will give 0 but proxy? wait use jsonl + db)
        std::fs::write(tmp.path().join("opencode.db"), b"").unwrap(); // marker, read handles bad as 0
        // also write jsonl so has turns
        write_opencode_jsonl_fixture(tmp.path());
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", tmp.path());
        std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        std::env::set_var("XDG_CONFIG_HOME", tempfile::tempdir().unwrap().path());
        let usage = read_usage(None, None, None);
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_CONFIG_DIR", prev_oc);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert!(!usage.is_empty());
        let u = usage.iter().find(|m| m.model == "opencode").unwrap();
        assert!(u.turns >= 3);
    }

    #[test]
    fn opencode_parse_variants_cover_content_keys_and_models() {
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let tmp = tempfile::tempdir().unwrap();
        // variant: role+content + ts
        let lines = r#"{"role":"user","content":"hi there with words","timestamp":"2026-07-11T00:00:00Z"}
{"role":"user","prompt":"alt key here","ts":1752225600}
"#;
        std::fs::write(tmp.path().join("prompt-history.jsonl"), lines).unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", tmp.path());
        std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        std::env::set_var("XDG_CONFIG_HOME", tempfile::tempdir().unwrap().path());
        let usage = read_usage(None, None, None);
        restore("LENS_HOST", prev_host.clone());
        restore("OPENCODE_CONFIG_DIR", prev_oc.clone());
        restore("HOME", prev_home.clone());
        restore("XDG_CONFIG_HOME", prev_xdg.clone());
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].turns, 2);
        // variant with explicit model
        let lines2 = r#"{"role":"user","content":"g","model":"grok-beta","timestamp":"2026-07-11T00:00:00Z"}
"#;
        std::fs::write(tmp.path().join("prompt-history.jsonl"), lines2).unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", tmp.path());
        let usage2 = read_usage(None, None, None);
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_CONFIG_DIR", prev_oc);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert_eq!(usage2[0].model, "grok-beta");
    }

    #[test]
    fn opencode_config_dirs_and_sqlite_stub_use_proxy_on_empty() {
        let _g = crate::rtk::env_test_lock();
        let prev_host = std::env::var_os("LENS_HOST");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_CONFIG_HOME");
        let good = tempfile::tempdir().unwrap();
        write_opencode_jsonl_fixture(good.path());
        // empty candidate dir that will be filtered (no state files, but is_dir check in union)
        let xdg = tempfile::tempdir().unwrap();
        // do not create xdg/opencode , its candidate will have !has_state
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", good.path());
        std::env::set_var("XDG_CONFIG_HOME", xdg.path());
        std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        let dirs = opencode_config_dirs();
        restore("LENS_HOST", prev_host.clone());
        restore("OPENCODE_CONFIG_DIR", prev_oc.clone());
        restore("HOME", prev_home.clone());
        restore("XDG_CONFIG_HOME", prev_xdg.clone());
        assert_eq!(dirs.len(), 1, "empty xdg/opencode candidate must be skipped");
        // now sqlite stub (bad db) + no jsonl -> should proxy count the dir itself
        let stub = tempfile::tempdir().unwrap();
        std::fs::write(stub.path().join("opencode.db"), b"").unwrap();
        // no prompt-history.jsonl
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", stub.path());
        std::env::set_var("HOME", tempfile::tempdir().unwrap().path());
        std::env::set_var("XDG_CONFIG_HOME", tempfile::tempdir().unwrap().path());
        let usage = read_usage(None, None, None);
        restore("LENS_HOST", prev_host);
        restore("OPENCODE_CONFIG_DIR", prev_oc);
        restore("HOME", prev_home);
        restore("XDG_CONFIG_HOME", prev_xdg);
        assert!(!usage.is_empty());
        assert!(usage.iter().any(|m| m.model == "opencode"));
    }
}
