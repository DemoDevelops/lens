//! RTK integration — the **headroom pattern**: ctxforge ships/installs the
//! prebuilt RTK binary (Apache-2.0, version-pinned) and surfaces RTK's *own*
//! measured shell-command savings. RTK owns Bash command rewriting via its own
//! Claude Code hook; ctxforge keeps its MCP / compaction / continuity lane and
//! **defers Bash to RTK** when RTK is active so the two hooks never double-wrap.
//!
//! Everything here is **additive and default-off**: with no RTK binary present,
//! every entry point is a cheap no-op and existing ctxforge behavior is unchanged.
//!
//! Layout (file ownership per `CTXFORGE_RTK_PLAN.md` §4 / `RTK_NOTES.md` §9):
//!   * [`install`] — download + install the pinned binary, register its hook (T1).
//!   * [`gain`]    — read `rtk gain --format json` and bridge deltas to the op log (T2).
//!   * [`rtk_active`] — tells the PreToolUse router to pass Bash through (T4).
//!
//! This is reached only via the `ctxforge rtk …` subcommand (a separate process);
//! it never touches the MCP server's JSON-RPC stdout.

pub mod gain;
pub mod install;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Pinned RTK release (headroom's pin; verified runnable on-machine in T0).
pub const RTK_VERSION: &str = "v0.28.2";

/// Managed binary file name (platform-specific).
#[cfg(windows)]
pub const RTK_EXE: &str = "rtk.exe";
/// Managed binary file name (platform-specific).
#[cfg(not(windows))]
pub const RTK_EXE: &str = "rtk";

// ---------------------------------------------------------------------------
// `rtk gain --format json` shape (mirrors RTK's ExportData / ExportSummary)
// ---------------------------------------------------------------------------

/// Deserialized `rtk gain --format json` output. Mirrors RTK's `ExportData`
/// (`rtk` `src/gain.rs` @ v0.28.2): a `summary` plus optional period breakdowns
/// that only appear with `--daily/--weekly/--monthly/--all`. Captured samples and
/// the field-type rationale live in `RTK_NOTES.md` §4; the parse is proven by
/// [`gain`]'s `gain_summary_deserializes_captured_sample` test.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GainSummary {
    pub summary: ExportSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<serde_json::Value>,
}

/// The `summary` block. RTK reports **tokens**, not bytes; `total_saved` is RTK's
/// own measured savings — surfaced verbatim, never re-estimated by ctxforge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExportSummary {
    pub total_commands: u64,
    pub total_input: u64,
    pub total_output: u64,
    /// RTK's own cumulative tokens-saved figure.
    pub total_saved: u64,
    pub avg_savings_pct: f64,
    pub total_time_ms: u64,
    pub avg_time_ms: u64,
}

// ---------------------------------------------------------------------------
// Path resolution (headroom-faithful global home; see RTK_NOTES.md §2)
// ---------------------------------------------------------------------------

/// The user's home directory (`$HOME`, else `$USERPROFILE`). `None` if unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// ctxforge's global home — `$CTXFORGE_HOME` if set, else `~/.ctxforge`. Mirrors
/// headroom's `workspace_dir()` (≙ `$HEADROOM_WORKSPACE_DIR` / `~/.headroom`).
/// Distinct from the per-project data dir `$CTXFORGE_DIR` (`<proj>/.ctxforge`).
pub fn home_root() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("CTXFORGE_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    home_dir().map(|h| h.join(".ctxforge"))
}

/// The managed bin dir — `home_root()/bin` (mirrors headroom `bin_dir()`).
pub fn bin_dir() -> Option<PathBuf> {
    home_root().map(|r| r.join("bin"))
}

/// Resolve the RTK binary: the managed install (`~/.ctxforge/bin/rtk`) if present,
/// else `rtk` on `PATH`. ctxforge is **managed-first** (its pinned binary is
/// authoritative once installed); headroom is PATH-first — see RTK_NOTES.md §2.
pub fn rtk_bin_path() -> Option<PathBuf> {
    if let Some(b) = bin_dir() {
        let p = b.join(RTK_EXE);
        if p.is_file() {
            return Some(p);
        }
    }
    which_rtk()
}

/// Scan `$PATH` for an `rtk` executable.
fn which_rtk() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(RTK_EXE);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// True if an RTK binary is resolvable (managed install or on `PATH`).
pub fn rtk_available() -> bool {
    rtk_bin_path().is_some()
}

/// Run the resolved RTK binary with `args`, capturing stdout/stderr. Errors if
/// RTK isn't installed or the process can't be spawned. Shared by install/status
/// (T1) and the gain bridge (T2).
pub fn run_rtk(args: &[&str]) -> Result<std::process::Output> {
    let bin = rtk_bin_path().context("rtk binary not found (run `ctxforge rtk install`)")?;
    Command::new(&bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", bin.display()))
}

// ---------------------------------------------------------------------------
// Hook registration detection (Claude settings.json; see RTK_NOTES.md §6)
// ---------------------------------------------------------------------------

/// The Claude config dir whose `settings.json` *this user's* Claude Code actually
/// reads — `$CLAUDE_CONFIG_DIR` if set, else `~/.claude`. This is where ctxforge
/// registers + detects the RTK hook, so the hook fires for the running Claude Code.
///
/// NB: `rtk init --global` (v0.28.2) ignores `$CLAUDE_CONFIG_DIR` and always writes
/// to `dirs::home_dir()/.claude` (see [`rtk_default_hook_script`]). So when
/// `$CLAUDE_CONFIG_DIR` differs (e.g. this machine's `~/.claude-personal`), ctxforge
/// patches the config-dir settings itself rather than relying on `rtk init`'s patch.
pub fn claude_config_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !d.is_empty() {
            return Some(PathBuf::from(d));
        }
    }
    home_dir().map(|h| h.join(".claude"))
}

/// Path to the Claude settings file ctxforge registers/detects the RTK hook in.
/// Honors `$CTXFORGE_CLAUDE_SETTINGS` (test seam), else [`claude_config_dir`]'s
/// `settings.json`.
pub fn claude_settings_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CTXFORGE_CLAUDE_SETTINGS") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    claude_config_dir().map(|d| d.join("settings.json"))
}

/// Where `rtk init` writes its hook script — always `dirs::home_dir()/.claude/
/// hooks/rtk-rewrite.sh` (rtk ignores `$CLAUDE_CONFIG_DIR`). ctxforge copies this
/// into the active config dir's `hooks/` when the two differ, so the hook is
/// self-contained under the dir the running Claude Code reads.
pub fn rtk_default_hook_script() -> Option<PathBuf> {
    home_dir().map(|h| h.join(".claude").join("hooks").join("rtk-rewrite.sh"))
}

/// True if RTK's PreToolUse hook is registered in Claude settings — any
/// `hooks.PreToolUse[].hooks[].command` mentioning `rtk` (covers v0.28.2's
/// `rtk-rewrite.sh` and older `rtk hook` markers). Missing/unreadable/malformed
/// settings read as "not registered".
pub fn rtk_hook_registered() -> bool {
    let Some(path) = claude_settings_path() else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return false;
    };
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => hook_mentions_rtk(&v),
        Err(_) => false,
    }
}

/// Does this settings object carry a PreToolUse hook whose command mentions `rtk`?
fn hook_mentions_rtk(settings: &serde_json::Value) -> bool {
    let Some(pre) = settings
        .get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
    else {
        return false;
    };
    pre.iter().any(|entry| {
        entry
            .get("hooks")
            .and_then(|h| h.as_array())
            .is_some_and(|hooks| {
                hooks.iter().any(|hk| {
                    hk.get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(|cmd| cmd.contains("rtk"))
                })
            })
    })
}

/// Should ctxforge defer Bash to RTK (RTK owns Bash rewriting)?
///
/// Env override wins — deterministic for tests, mirroring `CTXFORGE_ROUTING_MCP`:
/// `CTXFORGE_DEFER_BASH_TO_RTK` truthy ⇒ `true`, falsey ⇒ `false`. Otherwise
/// detect: RTK binary present **and** its hook registered in Claude settings.
///
/// `_data_dir` is reserved for future per-project scoping (kept symmetric with
/// [`crate::routing::mcp_ready`]); detection is currently global because RTK
/// installs its hook globally.
pub fn rtk_active(_data_dir: &Path) -> bool {
    if let Some(forced) = env_flag("CTXFORGE_DEFER_BASH_TO_RTK") {
        return forced;
    }
    rtk_available() && rtk_hook_registered()
}

/// Tri-state boolean env var: `Some(true)` / `Some(false)` / `None` (unset, blank,
/// or unrecognized).
fn env_flag(name: &str) -> Option<bool> {
    let raw = std::env::var(name).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "on" | "true" | "yes" => Some(true),
        "0" | "off" | "false" | "no" => Some(false),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `ctxforge rtk <command>` dispatcher
// ---------------------------------------------------------------------------

/// `ctxforge rtk <install|status|uninstall|sync>`. A separate process — its
/// stdout is its own response channel, never the MCP JSON-RPC stream.
pub fn run_cli(args: &[String]) -> Result<()> {
    match args.first().map(|s| s.as_str()) {
        Some("install") => install::install(),
        Some("status") => install::status(),
        Some("uninstall") => install::uninstall(),
        Some("sync") => gain::sync(),
        Some(other) => {
            eprintln!("ctxforge rtk: unknown subcommand '{other}'");
            print_usage();
            std::process::exit(2);
        }
        None => {
            print_usage();
            Ok(())
        }
    }
}

fn print_usage() {
    println!(
        "usage: ctxforge rtk <command>\n\
\n\
ctxforge ships and surfaces the RTK shell-command compressor (headroom pattern):\n\
RTK owns Bash rewriting via its own hook; ctxforge installs it and reports its savings.\n\
\n\
commands:\n  \
install     download + install the pinned RTK binary ({ver}) and register its Claude hook\n  \
status      show whether RTK is installed, its version, and hook registration\n  \
uninstall   remove RTK's Claude hook (rtk init --global --uninstall)\n  \
sync        read `rtk gain` and append shell-savings deltas to the ctxforge op log\n",
        ver = RTK_VERSION
    );
}

/// Shared guard serializing the unit tests that mutate the process-global
/// `CTXFORGE_HOME` env var (env is global; `cargo test` runs in parallel). Used
/// here and by `obs::stats` tests. Poison-tolerant: a panicked holder still yields
/// the guard so one failing test doesn't cascade.
#[cfg(test)]
pub(crate) fn env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_detection_matches_rtk_commands_only() {
        // v0.28.2 marker (rtk-rewrite.sh) and the older `rtk hook` marker both match.
        for cmd in ["/Users/x/.claude/hooks/rtk-rewrite.sh", "rtk hook claude"] {
            let s = json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": cmd}]}
            ]}});
            assert!(hook_mentions_rtk(&s), "should detect rtk hook: {cmd}");
        }
        // A non-rtk PreToolUse hook (e.g. ctxforge's own) must NOT match.
        let other = json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "ctxforge hook claude PreToolUse"}]}
        ]}});
        assert!(!hook_mentions_rtk(&other));
        // No hooks at all.
        assert!(!hook_mentions_rtk(&json!({})));
    }

    #[test]
    fn rtk_active_env_override_wins() {
        let dir = std::env::temp_dir();
        // Truthy / falsey overrides short-circuit before any binary/hook detection.
        std::env::set_var("CTXFORGE_DEFER_BASH_TO_RTK", "1");
        assert!(rtk_active(&dir));
        std::env::set_var("CTXFORGE_DEFER_BASH_TO_RTK", "off");
        assert!(!rtk_active(&dir));
        std::env::remove_var("CTXFORGE_DEFER_BASH_TO_RTK");
    }

    #[test]
    fn home_root_honors_ctxforge_home_override() {
        let _g = env_test_lock();
        std::env::set_var("CTXFORGE_HOME", "/tmp/ctxforge-home-test");
        assert_eq!(
            home_root().unwrap(),
            PathBuf::from("/tmp/ctxforge-home-test")
        );
        assert_eq!(
            bin_dir().unwrap(),
            PathBuf::from("/tmp/ctxforge-home-test/bin")
        );
        std::env::remove_var("CTXFORGE_HOME");
    }
}
