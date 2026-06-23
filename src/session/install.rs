//! `lens session <install|uninstall|status>` — register/remove the
//! lifecycle hooks in Claude Code's `settings.json`.
//!
//! Install is atomic (refuses to run alongside Context Mode's hooks, which would
//! double-fire on the same lifecycle events) and reversible (uninstall removes
//! only lens's entries, leaving any other hooks untouched).

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::store::SessionStore;
use crate::index::Index;

/// The five lifecycle events lens registers, with their settings matcher.
/// Empty matcher = fire for all tools / always.
const EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "PreCompact",
    "SessionStart",
];

/// Substring identifying a lens-owned hook command.
const MARKER: &str = "hook claude";
const SELF_MARKER: &str = "lens";

/// CLI entry: `args` is everything after `session`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    let settings = settings_path(settings_override(args))?;
    let bin = std::env::current_exe()
        .context("resolving lens binary path")?
        .to_string_lossy()
        .to_string();

    match sub {
        "install" => {
            match install(&settings, &bin) {
                Ok(()) => {
                    println!("lens session hooks installed at {}", settings.display());
                    println!("  binary: {bin}");
                    println!("\nInstalling RTK shell compressor...");
                    if let Err(e) = crate::rtk::install::install() {
                        eprintln!("warning: RTK install failed: {e:#}");
                        eprintln!("  Run `lens rtk install` manually to retry.");
                    }
                    println!("\nNext: uninstall Context Mode if you have it, then verify with `lens session status`.");
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "uninstall" => {
            let n = uninstall(&settings)?;
            println!(
                "removed {n} lens hook entr{} from {}",
                if n == 1 { "y" } else { "ies" },
                settings.display()
            );
            Ok(())
        }
        "status" => {
            let r = status(&settings);
            print_status(&r);
            Ok(())
        }
        other => {
            eprintln!("unknown session subcommand '{other}' (use install|uninstall|status)");
            std::process::exit(2);
        }
    }
}

/// `--config-dir <dir>` -> `<dir>/settings.json`; `--settings <file>` -> that
/// file. Lets you target a specific account's config (e.g. `~/.claude-personal`
/// vs the work `~/.claude`) without relying on ambient env. Flags follow the
/// subcommand: `lens session install --config-dir ~/.claude-personal`.
fn settings_override(args: &[String]) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config-dir" => return args.get(i + 1).map(|d| PathBuf::from(d).join("settings.json")),
            "--settings" => return args.get(i + 1).map(PathBuf::from),
            _ => {}
        }
        i += 1;
    }
    None
}

/// Resolve the settings.json to write, by precedence: an explicit CLI override
/// (`--config-dir`/`--settings`), then `LENS_SETTINGS`, then the dir THIS Claude
/// Code reads (`$CLAUDE_CONFIG_DIR` if set, e.g. `~/.claude-personal`, else
/// `~/.claude`). Mirrors the RTK side (`rtk::claude_settings_path`) so session +
/// rtk hooks land in the same settings.json.
fn settings_path(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("LENS_SETTINGS") {
        return Ok(PathBuf::from(p));
    }
    crate::rtk::claude_config_dir()
        .map(|d| d.join("settings.json"))
        .ok_or_else(|| anyhow!("HOME not set"))
}

fn load(settings: &Path) -> Result<Value> {
    if !settings.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(settings)
        .with_context(|| format!("reading {}", settings.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", settings.display()))
}

fn save(settings: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = settings.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let pretty = serde_json::to_string_pretty(value)?;
    std::fs::write(settings, pretty + "\n")
        .with_context(|| format!("writing {}", settings.display()))?;
    Ok(())
}

/// Install the five hooks. Errors (refuses) if Context Mode hooks are present.
pub fn install(settings: &Path, bin: &str) -> Result<()> {
    let mut root = load(settings)?;
    if context_mode_present(&root) {
        return Err(anyhow!(
            "Context Mode hooks detected — uninstall Context Mode first (`/plugin uninstall context-mode`) to avoid double-firing session hooks."
        ));
    }

    // Ensure hooks object.
    if !root.get("hooks").map(|h| h.is_object()).unwrap_or(false) {
        root["hooks"] = json!({});
    }

    // Remove any stale lens entries first (idempotent install).
    strip_lens(&mut root);

    for event in EVENTS {
        let cmd = format!("\"{bin}\" hook claude {event}");
        let group = json!({
            "matcher": "",
            "hooks": [ { "type": "command", "command": cmd } ]
        });
        let arr = root["hooks"]
            .as_object_mut()
            .unwrap()
            .entry(event.to_string())
            .or_insert_with(|| json!([]));
        if let Some(a) = arr.as_array_mut() {
            a.push(group);
        }
    }

    save(settings, &root)
}

/// Remove only lens's hook entries. Returns how many groups were removed.
pub fn uninstall(settings: &Path) -> Result<usize> {
    let mut root = load(settings)?;
    let removed = strip_lens(&mut root);
    save(settings, &root)?;
    Ok(removed)
}

/// Remove every lens-owned hook group from `root`, pruning empty arrays.
/// Returns the number of groups removed.
fn strip_lens(root: &mut Value) -> usize {
    let mut removed = 0;
    let hooks = match root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        Some(h) => h,
        None => return 0,
    };
    let mut empty_events = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        if let Some(arr) = groups.as_array_mut() {
            let before = arr.len();
            arr.retain(|g| !group_is_lens(g));
            removed += before - arr.len();
            if arr.is_empty() {
                empty_events.push(event.clone());
            }
        }
    }
    for e in empty_events {
        hooks.remove(&e);
    }
    removed
}

fn group_is_lens(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| hs.iter().any(command_is_lens))
        .unwrap_or(false)
}

fn command_is_lens(hook: &Value) -> bool {
    hook.get("command")
        .and_then(|c| c.as_str())
        .map(|c| c.contains(SELF_MARKER) && c.contains(MARKER))
        .unwrap_or(false)
}

/// Detect Context Mode's lifecycle hooks: either an enabled `context-mode`
/// plugin, or any hook command in settings referencing context-mode.
pub fn context_mode_present(root: &Value) -> bool {
    // 1. enabledPlugins / enabledPlugins-style maps with a context-mode key.
    for key in ["enabledPlugins", "enabled_plugins"] {
        if let Some(map) = root.get(key).and_then(|v| v.as_object()) {
            for (name, enabled) in map {
                if name.starts_with("context-mode") && enabled.as_bool().unwrap_or(false) {
                    return true;
                }
            }
        }
    }
    // 2. Any hook command string mentioning context-mode.
    if let Some(hooks) = root.get("hooks").and_then(|h| h.as_object()) {
        for groups in hooks.values() {
            if let Some(arr) = groups.as_array() {
                for g in arr {
                    if let Some(hs) = g.get("hooks").and_then(|h| h.as_array()) {
                        for h in hs {
                            if h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("context-mode"))
                                .unwrap_or(false)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

/// Result of a `session status` check.
#[derive(Debug)]
pub struct Status {
    pub installed_events: Vec<String>,
    pub conflict: bool,
    pub store_ok: bool,
    pub fts_ok: bool,
}

/// Inspect hook installation + backing stores.
pub fn status(settings: &Path) -> Status {
    let root = load(settings).unwrap_or_else(|_| json!({}));
    let mut installed_events = Vec::new();
    if let Some(hooks) = root.get("hooks").and_then(|h| h.as_object()) {
        for (event, groups) in hooks {
            if let Some(arr) = groups.as_array() {
                if arr.iter().any(group_is_lens) {
                    installed_events.push(event.clone());
                }
            }
        }
    }
    installed_events.sort();

    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let data_dir = super::resolve_data_dir(&project);
    let store_ok = SessionStore::open(&data_dir).is_ok();
    let fts_ok = Index::open(&data_dir).and_then(|i| i.chunk_count()).is_ok();

    Status {
        installed_events,
        conflict: context_mode_present(&root),
        store_ok,
        fts_ok,
    }
}

fn print_status(s: &Status) {
    let mark = |b: bool| if b { "ok" } else { "FAIL" };
    println!("lens session status");
    if s.installed_events.is_empty() {
        println!("  hooks installed : none (run `lens session install`)");
    } else {
        println!(
            "  hooks installed : {} ({})",
            s.installed_events.len(),
            s.installed_events.join(", ")
        );
    }
    println!(
        "  context-mode    : {}",
        if s.conflict {
            "PRESENT — conflict! uninstall it"
        } else {
            "not detected"
        }
    );
    println!("  event store     : {}", mark(s.store_ok));
    println!("  FTS5 index      : {}", mark(s.fts_ok));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write(path: &Path, v: &Value) {
        std::fs::write(path, serde_json::to_string_pretty(v).unwrap()).unwrap();
    }

    #[test]
    fn settings_override_parses_config_dir_and_settings() {
        let s = |a: &[&str]| super::settings_override(&a.iter().map(|x| x.to_string()).collect::<Vec<_>>());
        assert_eq!(
            s(&["install", "--config-dir", "/x/.claude-personal"]),
            Some(PathBuf::from("/x/.claude-personal/settings.json"))
        );
        assert_eq!(
            s(&["install", "--settings", "/x/custom.json"]),
            Some(PathBuf::from("/x/custom.json"))
        );
        assert_eq!(s(&["install"]), None);
        assert_eq!(s(&["install", "--config-dir"]), None); // missing value
    }

    #[test]
    fn install_adds_five_hooks_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        install(&settings, "/usr/bin/lens").unwrap();
        let root = load(&settings).unwrap();
        let hooks = root["hooks"].as_object().unwrap();
        for ev in EVENTS {
            assert!(hooks.contains_key(ev), "missing {ev}");
        }
        // command embeds the absolute binary path.
        let cmd = hooks["PostToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("/usr/bin/lens"));
        assert!(cmd.contains("hook claude PostToolUse"));

        // Re-install: still exactly one group per event.
        install(&settings, "/usr/bin/lens").unwrap();
        let root2 = load(&settings).unwrap();
        assert_eq!(root2["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn install_refuses_on_context_mode_conflict() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        write(
            &settings,
            &json!({
                "enabledPlugins": { "context-mode@context-mode": true }
            }),
        );
        let err = install(&settings, "/usr/bin/lens").unwrap_err();
        assert!(err.to_string().contains("Context Mode hooks detected"));
    }

    #[test]
    fn install_succeeds_when_context_mode_absent() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        write(&settings, &json!({ "enabledPlugins": { "other@x": true } }));
        assert!(install(&settings, "/usr/bin/lens").is_ok());
    }

    #[test]
    fn uninstall_removes_only_lens_leaving_others() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        // Pre-existing unrelated hook.
        write(
            &settings,
            &json!({
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash", "hooks": [ { "type": "command", "command": "rtk hook claude" } ] }
                    ]
                }
            }),
        );
        install(&settings, "/usr/bin/lens").unwrap();
        let removed = uninstall(&settings).unwrap();
        assert_eq!(removed, 5);
        let root = load(&settings).unwrap();
        // The unrelated rtk hook survives.
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], "rtk hook claude");
        // lens-only events were pruned.
        assert!(root["hooks"].get("PreCompact").is_none());
    }

    #[test]
    fn status_reports_installed_and_conflict() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        install(&settings, "/usr/bin/lens").unwrap();
        let s = status(&settings);
        assert_eq!(s.installed_events.len(), 5);
        assert!(!s.conflict);
    }
}
