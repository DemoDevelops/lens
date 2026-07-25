//! RTK integration — lens **detects** an RTK the user installed themselves and
//! surfaces RTK's *own* measured shell-command savings. RTK owns Bash command
//! rewriting via its own Claude Code hook; lens keeps its MCP / compaction /
//! continuity lane and **defers Bash to RTK** when RTK is active so the two hooks
//! never double-wrap.
//!
//! lens does **not** package, download, pin, or install RTK, and does not own its
//! hook — install it from <https://github.com/rtk-ai/rtk> and register the hook
//! with `rtk init`. Decoupling the two means RTK upgrades on its own cadence
//! instead of being frozen at whatever version lens last vendored.
//!
//! Everything here is **additive and default-off**: with no RTK binary present,
//! every entry point is a cheap no-op and existing lens behavior is unchanged.
//!
//! Layout:
//!   * [`gain`]       — read `rtk gain --format json` and bridge deltas to the op log.
//!   * [`rtk_active`] — tells the PreToolUse router to pass Bash through.
//!   * [`status`]     — report the detected binary, version, and hook registration.
//!
//! This is reached only via the `lens rtk …` subcommand (a separate process);
//! it never touches the MCP server's JSON-RPC stdout.

pub mod gain;

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::client;

/// RTK binary file name (platform-specific).
#[cfg(windows)]
pub const RTK_EXE: &str = "rtk.exe";
/// RTK binary file name (platform-specific).
#[cfg(not(windows))]
pub const RTK_EXE: &str = "rtk";

/// Where to get RTK, shown whenever it isn't found.
pub const RTK_INSTALL_HINT: &str = "install it from https://github.com/rtk-ai/rtk";

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
/// own measured savings — surfaced verbatim, never re-estimated by lens.
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

/// lens's global home — `$LENS_HOME` if set, else `~/.lens`. Mirrors
/// headroom's `workspace_dir()` (≙ `$HEADROOM_WORKSPACE_DIR` / `~/.headroom`).
/// Distinct from the per-project data dir `$LENS_DIR` (`<proj>/.lens`).
pub fn home_root() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("LENS_HOME") {
        if !h.is_empty() {
            return Some(PathBuf::from(h));
        }
    }
    home_dir().map(|h| h.join(".lens"))
}

/// Resolve the RTK binary: whatever `rtk` is on `PATH`. lens no longer keeps a
/// managed copy to prefer, so the user's own install is the only one.
///
/// Honors `$LENS_RTK_BIN` (test seam, mirroring `$LENS_CLAUDE_SETTINGS`) so a test
/// can pin a stub without mutating the process-global `PATH`.
pub fn rtk_bin_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LENS_RTK_BIN") {
        if !p.is_empty() {
            let p = PathBuf::from(p);
            return p.is_file().then_some(p);
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

/// RTK (the shell compressor + hook) is supported only under Claude today.
/// For opencode we skip install and emit: "RTK is Claude-specific today; shell savings via opencode plugins TBD".
pub fn is_rtk_supported() -> bool {
    client::is_claude()
}

/// Run the resolved RTK binary with `args`, capturing stdout/stderr. Errors if
/// RTK isn't installed or the process can't be spawned. Shared by [`status`] and
/// the gain bridge.
pub fn run_rtk(args: &[&str]) -> Result<std::process::Output> {
    let bin =
        rtk_bin_path().with_context(|| format!("rtk binary not found on PATH — {RTK_INSTALL_HINT}"))?;
    Command::new(&bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", bin.display()))
}

// ---------------------------------------------------------------------------
// Hook registration detection (Claude settings.json; see RTK_NOTES.md §6)
// ---------------------------------------------------------------------------

/// The Claude config dir whose `settings.json` *this user's* Claude Code actually
/// reads — `$CLAUDE_CONFIG_DIR` if set, else `~/.claude`. This is where lens
/// **detects** the RTK hook that `rtk init` registered.
///
/// NB: `rtk init --global` ignores `$CLAUDE_CONFIG_DIR` and always writes to
/// `dirs::home_dir()/.claude`. When `$CLAUDE_CONFIG_DIR` differs from `~/.claude`,
/// run `rtk init` with it exported (or register the hook by hand) so the hook
/// lands in the dir the running Claude Code reads.
pub fn claude_config_dir() -> Option<PathBuf> {
    client::config_dir_for(client::Host::Claude)
}

/// Path to the Claude settings file lens detects the RTK hook in. Honors
/// `$LENS_CLAUDE_SETTINGS` (test seam), else [`claude_config_dir`]'s
/// `settings.json`.
pub fn claude_settings_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("LENS_CLAUDE_SETTINGS") {
        if !p.is_empty() {
            return Some(PathBuf::from(p));
        }
    }
    claude_config_dir().map(|d| d.join("settings.json"))
}

/// True if RTK's PreToolUse hook is registered in Claude settings — any
/// `hooks.PreToolUse[].hooks[].command` mentioning `rtk` (covers `rtk-rewrite.sh`
/// and older `rtk hook` markers). Missing/unreadable/malformed settings read as
/// "not registered".
pub fn rtk_hook_registered() -> bool {
    if !client::is_claude() {
        return false;
    }
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

/// Should lens defer Bash to RTK (RTK owns Bash rewriting)?
///
/// Env override wins — deterministic for tests, mirroring `LENS_ROUTING_MCP`:
/// `LENS_DEFER_BASH_TO_RTK` truthy ⇒ `true`, falsey ⇒ `false`. Otherwise
/// detect: RTK binary present **and** its hook registered in Claude settings.
///
/// `_data_dir` is reserved for future per-project scoping (kept symmetric with
/// [`crate::routing::mcp_ready`]); detection is currently global because RTK
/// installs its hook globally.
pub fn rtk_active(_data_dir: &Path) -> bool {
    if !client::is_claude() {
        return false;
    }
    if let Some(forced) = env_flag("LENS_DEFER_BASH_TO_RTK") {
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
// `lens rtk <command>` dispatcher
// ---------------------------------------------------------------------------

/// `lens rtk <status|sync>`. A separate process — its stdout is its own response
/// channel, never the MCP JSON-RPC stream.
pub fn run_cli(args: &[String]) -> Result<()> {
    if !is_rtk_supported() {
        println!("RTK is Claude-specific today; shell savings via opencode plugins TBD.");
        return Ok(());
    }
    match args.first().map(|s| s.as_str()) {
        Some("status") => status(),
        Some("sync") => gain::sync(),
        Some(other) => {
            eprintln!("lens rtk: unknown subcommand '{other}'");
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
        "usage: lens rtk <command>\n\
\n\
lens surfaces the savings of an RTK you installed yourself: RTK owns Bash\n\
rewriting via its own hook, lens reads its `gain` numbers. lens does not\n\
install, pin, or upgrade RTK — {hint}.\n\
\n\
commands:\n  \
status      show whether RTK is on PATH, its version, and hook registration\n  \
sync        read `rtk gain` and append shell-savings deltas to the lens op log\n",
        hint = RTK_INSTALL_HINT
    );
}

// ---------------------------------------------------------------------------
// `lens rtk status`
// ---------------------------------------------------------------------------

/// Run `<bin> --version` and return its trimmed stdout, or `Err` if it can't be
/// spawned or exits nonzero.
fn run_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", bin.display()))?;
    if !out.status.success() {
        anyhow::bail!(
            "{} --version exited {}: {}",
            bin.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Does `name` resolve on `PATH`? Mirrors the hook's own `command -v <name>`
/// check, so `status` reports exactly what the live hook will find.
fn cmd_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Count PreToolUse hook entries whose command mentions `rtk`. Drives `lens
/// setup`'s check that RTK's hook isn't registered more than once (two entries
/// double-fire the rewrite on every Bash call).
pub fn count_rtk_hooks(settings: &Path) -> usize {
    let Ok(raw) = std::fs::read_to_string(settings) else {
        return 0;
    };
    let Ok(root) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return 0;
    };
    root.get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    e.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .is_some_and(|c| c.contains("rtk"))
                        })
                    })
                })
                .count()
        })
        .unwrap_or(0)
}

/// Report detected state: binary path, `--version`, hook registration, whether the
/// hook can rewrite live (rtk on PATH + jq), and a one-line gain summary.
/// Best-effort — never errors when RTK is absent.
pub fn status() -> Result<()> {
    if !client::is_claude() {
        println!("RTK is Claude-specific today; shell savings via opencode plugins TBD.");
        return Ok(());
    }
    match rtk_bin_path() {
        Some(bin) => {
            println!("rtk binary: {}", bin.display());
            match run_version(&bin) {
                Ok(v) => println!("version:    {v}"),
                Err(e) => println!("version:    (failed: {e:#})"),
            }
        }
        None => println!("rtk binary: not on PATH ({RTK_INSTALL_HINT})"),
    }

    println!(
        "hook:       {}",
        if rtk_hook_registered() {
            "registered in Claude settings"
        } else {
            "not registered (run `rtk init`)"
        }
    );

    // The hook shells out to `rtk` and `jq`, and runs outside your interactive
    // shell — so both must resolve on the PATH that hook inherits.
    let rtk_ok = rtk_available();
    let jq = cmd_exists("jq");
    if rtk_ok && jq {
        println!("rewrite:    live (rtk + jq on PATH)");
    } else {
        let mut needs = Vec::new();
        if !rtk_ok {
            needs.push(format!("install rtk ({RTK_INSTALL_HINT})"));
        }
        if !jq {
            needs.push("install jq".to_string());
        }
        println!(
            "rewrite:    inactive — to enable live rewriting: {}",
            needs.join("; ")
        );
    }

    let gain_line = match gain::read_gain(gain::Scope::Global) {
        Ok(g) => format!(
            "{} commands, {} tokens saved ({:.1}% avg)",
            g.summary.total_commands, g.summary.total_saved, g.summary.avg_savings_pct
        ),
        Err(_) => "n/a".to_string(),
    };
    println!("gain:       {gain_line}");

    Ok(())
}

/// Shared guard serializing the unit tests that mutate the process-global
/// `LENS_HOME` env var (env is global; `cargo test` runs in parallel). Used
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
        // rtk's own marker (rtk-rewrite.sh) and the older `rtk hook` marker both match.
        for cmd in ["/Users/x/.claude/hooks/rtk-rewrite.sh", "rtk hook claude"] {
            let s = json!({"hooks": {"PreToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": cmd}]}
            ]}});
            assert!(hook_mentions_rtk(&s), "should detect rtk hook: {cmd}");
        }
        // A non-rtk PreToolUse hook (e.g. lens's own) must NOT match.
        let other = json!({"hooks": {"PreToolUse": [
            {"matcher": "Bash", "hooks": [{"type": "command", "command": "lens hook claude PreToolUse"}]}
        ]}});
        assert!(!hook_mentions_rtk(&other));
        // No hooks at all.
        assert!(!hook_mentions_rtk(&json!({})));
    }

    #[test]
    fn rtk_active_env_override_wins() {
        let dir = std::env::temp_dir();
        // Truthy / falsey overrides short-circuit before any binary/hook detection.
        std::env::set_var("LENS_DEFER_BASH_TO_RTK", "1");
        assert!(rtk_active(&dir));
        std::env::set_var("LENS_DEFER_BASH_TO_RTK", "off");
        assert!(!rtk_active(&dir));
        std::env::remove_var("LENS_DEFER_BASH_TO_RTK");
    }

    #[test]
    fn home_root_honors_lens_home_override() {
        let _g = env_test_lock();
        std::env::set_var("LENS_HOME", "/tmp/lens-home-test");
        assert_eq!(home_root().unwrap(), PathBuf::from("/tmp/lens-home-test"));
        std::env::remove_var("LENS_HOME");
    }

    #[test]
    fn count_rtk_hooks_counts_rtk_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        let v = json!({ "hooks": { "PreToolUse": [
            { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/h/.claude/hooks/rtk-rewrite.sh" } ] },
            { "matcher": "Bash", "hooks": [ { "type": "command", "command": "/h/.claude-personal/hooks/rtk-rewrite.sh" } ] },
            { "matcher": "", "hooks": [ { "type": "command", "command": "lens hook claude PreToolUse" } ] }
        ] } });
        std::fs::write(&settings, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        // The duplicate spelling is exactly what `lens setup` must flag.
        assert_eq!(count_rtk_hooks(&settings), 2);
        // Missing file reads as zero, not an error.
        assert_eq!(count_rtk_hooks(&dir.path().join("nope.json")), 0);
    }
}
