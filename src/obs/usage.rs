//! Claude Code JSONL usage reader — the real per-model token/turn/cost mix
//! behind the dashboard's "Actual Usage" pricing mode.
//!
//! Claude Code writes one JSONL transcript per session under
//! `<config dir>/projects/**/*.jsonl` (`<config dir>` is `$CLAUDE_CONFIG_DIR`,
//! else `$XDG_CONFIG_HOME/claude`, else `~/.claude`). Each line is a
//! loosely-typed event; only `assistant` turns carry `message.usage` and a
//! `model`. This module globs those files, tolerates every other line shape by
//! skipping it, dedups resumed/rewritten turns by `(message.id, requestId)`,
//! and aggregates the survivors into a per-model window summary.
//!
//! Read-only and best-effort: a missing config dir or an unreadable file
//! yields an empty result, never an error — this feeds a dashboard panel, not
//! a correctness-critical path.

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

/// Read and aggregate Claude Code's own usage transcripts into a per-model
/// summary, restricted to `[since, until)` (unix seconds; `None` is
/// unbounded on that side) and, if `cwd_filter` is set, to lines whose `cwd`
/// is under it. Returns one entry per distinct model, or an empty vec if no
/// Claude Code config dir is found.
pub fn read_usage(
    since: Option<i64>,
    until: Option<i64>,
    cwd_filter: Option<&Path>,
) -> Vec<ModelUsage> {
    let Some(config_dir) = claude_config_dir() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut agg: BTreeMap<String, ModelUsage> = BTreeMap::new();
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
    agg.into_values().collect()
}

/// Resolve the Claude Code config dir: `$CLAUDE_CONFIG_DIR`, else
/// `$XDG_CONFIG_HOME/claude`, else `~/.claude`. `None` unless the resolved
/// dir has a `projects/` subdir (the signal it actually holds transcripts).
fn claude_config_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(|x| PathBuf::from(x).join("claude")))
        .or_else(|| home_dir().map(|h| h.join(".claude")))?;
    dir.join("projects").is_dir().then_some(dir)
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

    /// Point `CLAUDE_CONFIG_DIR` at a fresh temp dir holding the fixture for
    /// the duration of `f`, then restore the prior value. `CLAUDE_CONFIG_DIR`
    /// is process-global; serialize with `env_test_lock` like the other
    /// env-mutating tests in this crate.
    fn with_fixture<T>(f: impl FnOnce() -> T) -> T {
        let _g = crate::rtk::env_test_lock();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path());
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        let out = f();
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        out
    }

    #[test]
    fn claude_config_dir_resolves_via_env_override() {
        let _g = crate::rtk::env_test_lock();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        let tmp = tempfile::tempdir().unwrap();
        write_fixture(tmp.path());
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        let resolved = claude_config_dir();
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert_eq!(resolved.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn claude_config_dir_none_without_projects_subdir() {
        let _g = crate::rtk::env_test_lock();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        let tmp = tempfile::tempdir().unwrap(); // no projects/ written
        std::env::set_var("CLAUDE_CONFIG_DIR", tmp.path());
        let resolved = claude_config_dir();
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }
        assert!(resolved.is_none());
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
}
