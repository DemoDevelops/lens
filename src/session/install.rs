//! `lens session <install|uninstall|status>` — register/remove the
//! lifecycle hooks in Claude Code's `settings.json` (or install commands for opencode).
//!
//! Host-aware via `--client` / `LENS_HOST` (from T1). For opencode: populates
//! commands/ plus the plugins/lens.js lifecycle bridge. Claude behavior byte-identical.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::store::SessionStore;
use crate::index::Index;
use crate::client::{self, Host};

/// The five lifecycle events lens registers, with their settings matcher.
/// Empty matcher = fire for all tools / always.
const EVENTS: [&str; 5] = [
    "PreToolUse",
    "PostToolUse",
    "UserPromptSubmit",
    "PreCompact",
    "SessionStart",
];

/// Substring identifying a lens-owned hook command (Claude).
const MARKER: &str = "hook claude";
/// Substring for opencode (future hook support; commands path today).
const OPENCODE_MARKER: &str = "hook opencode";
const SELF_MARKER: &str = "lens";

/// Bundled slash commands, embedded at compile time so a sent binary can install them
/// with no repo checkout. Each is written to `<config dir>/commands/<file>` on install,
/// removed on uninstall.
const BUNDLED_COMMANDS: &[(&str, &str)] = &[
    ("dashboard.md", include_str!("../../assets/commands/dashboard.md")),
    ("warmup.md", include_str!("../../assets/commands/warmup.md")),
];

/// Bundled opencode lifecycle-bridge plugin (see assets/opencode/lens.js),
/// written to `<config dir>/plugins/lens.js` with the placeholders filled at
/// install time. It shells opencode's tool/session hooks into
/// `lens hook opencode <event>`, the same contract the Claude hooks use.
const OPENCODE_PLUGIN: &str = include_str!("../../assets/opencode/lens.js");
const OPENCODE_PLUGIN_FILE: &str = "lens.js";

/// CLI entry: `args` is everything after `session`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    let host = client::detect_host();
    let target = resolve_target(host, args)?;
    let bin = std::env::current_exe()
        .context("resolving lens binary path")?
        .to_string_lossy()
        .to_string();

    match sub {
        "install" => {
            match install_for(host, &target, &bin) {
                Ok(()) => {
                    if host == Host::Claude {
                        println!("lens session hooks installed at {}", target.display());
                        println!("  binary: {bin}");
                        println!("\nInstalling RTK shell compressor...");
                        if let Err(e) = crate::rtk::install::install() {
                            eprintln!("warning: RTK install failed: {e:#}");
                            eprintln!("  Run `lens rtk install` manually to retry.");
                        }
                        println!("\nNext: uninstall Context Mode if you have it, then verify with `lens session status`.");
                    } else {
                        println!("lens session commands + lifecycle plugin installed at {}", target.display());
                        println!("  binary: {bin}");
                        println!("\nNote: lifecycle events bridge through plugins/lens.js (tool before/after, prompt, session events); RTK stays Claude-only.");
                    }
                    Ok(())
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        "uninstall" => {
            let n = uninstall_for(host, &target)?;
            if host == Host::Claude {
                println!(
                    "removed {n} lens hook entr{} from {}",
                    if n == 1 { "y" } else { "ies" },
                    target.display()
                );
            } else {
                println!(
                    "removed lens commands{} from {}",
                    if n > 0 { " and the mcp.lens entry" } else { "" },
                    target.display()
                );
            }
            Ok(())
        }
        "status" => {
            let r = status_for(host, &target);
            print_status_for(host, &r);
            Ok(())
        }
        other => {
            eprintln!("unknown session subcommand '{other}' (use install|uninstall|status)");
            std::process::exit(2);
        }
    }
}

/// `--config-dir <dir>` -> `<dir>/settings.json`; `--settings <file>` -> that
/// file. (Claude-oriented; for opencode `--config-dir` is interpreted as config root
/// via resolve_target stripping). Flags follow the subcommand:
/// `lens session install --config-dir <dir>`.
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

/// Resolve the target for this host: for Claude the settings.json path,
/// for opencode the config dir (so commands/ lives directly under it).
/// Honors --config-dir/--settings (with host adjustment for opencode), LENS_* envs,
/// and client::config_dir_for (which honors OPENCODE_CONFIG_DIR etc).
fn resolve_target(host: Host, args: &[String]) -> Result<PathBuf> {
    let ov = settings_override(args);
    match host {
        Host::Claude => settings_path(ov),
        Host::Opencode => opencode_config_dir(ov),
    }
}

/// Resolve the settings.json to write, by precedence: an explicit CLI override
/// (`--config-dir`/`--settings`), then `LENS_SETTINGS`, then the dir THIS Claude
/// Code reads (`$CLAUDE_CONFIG_DIR` if set, else
/// `~/.claude`). Mirrors the RTK side (`rtk::claude_settings_path`) so session +
/// rtk hooks land in the same settings.json.
/// (kept for Claude byte-compat; generalized callers use resolve_target)
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

/// For opencode: return config root (never a settings.json). Strips accidental
/// /settings.json suffix that settings_override adds for --config-dir.
fn opencode_config_dir(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(mut p) = override_path {
        if p.ends_with("settings.json") {
            if let Some(parent) = p.parent() {
                p = parent.to_path_buf();
            }
        }
        // if --settings pointed at json for opencode, use its dir; else the path
        if p.extension().is_some_and(|e| e == "json") {
            if let Some(parent) = p.parent() {
                p = parent.to_path_buf();
            }
        }
        return Ok(p);
    }
    if let Some(p) = std::env::var_os("LENS_SETTINGS") {
        let pb = PathBuf::from(p);
        return Ok(pb.parent().unwrap_or(&pb).to_path_buf());
    }
    client::config_dir_for(Host::Opencode)
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

/// Install the five hooks (Claude). Errors (refuses) if Context Mode hooks are present.
/// Kept for direct calls / tests; host-aware entry is install_for.
pub fn install(settings: &Path, bin: &str) -> Result<()> {
    install_for(Host::Claude, settings, bin)
}

fn install_for(host: Host, target: &Path, bin: &str) -> Result<()> {
    match host {
        Host::Claude => install_claude(target, bin),
        Host::Opencode => install_opencode(target, bin),
    }
}

fn install_claude(settings: &Path, bin: &str) -> Result<()> {
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

    save(settings, &root)?;
    if let Some(p) = settings.parent() {
        install_commands(p)?;
    }
    Ok(())
}

/// For opencode: bundled commands + the lifecycle-bridge plugin. Never touches
/// settings.json or any Claude hook groups.
fn install_opencode(config_dir: &Path, bin: &str) -> Result<()> {
    install_opencode_assets(config_dir, bin)
}

/// Write the bundled commands and the lifecycle plugin for opencode. The
/// plugin's routing level comes from the registered mcp.lens entry when
/// present (setup registers MCP first), else `full`.
pub fn install_opencode_assets(config_dir: &Path, bin: &str) -> Result<()> {
    install_commands(config_dir)?;
    let routing =
        crate::setup::read_opencode_routing().unwrap_or_else(|| "full".to_string());
    let plugin_dir = config_dir.join("plugins");
    std::fs::create_dir_all(&plugin_dir)
        .with_context(|| format!("creating {}", plugin_dir.display()))?;
    let content = OPENCODE_PLUGIN
        .replace("__LENS_BIN__", bin)
        .replace("__LENS_ROUTING__", &routing);
    let path = plugin_dir.join(OPENCODE_PLUGIN_FILE);
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Write the bundled slash commands into `<config dir>/commands/`.
/// (generalized to take config dir directly; claude callers pass .parent())
pub fn install_commands(config_dir: &Path) -> Result<()> {
    let cmd_dir = config_dir.join("commands");
    std::fs::create_dir_all(&cmd_dir)
        .with_context(|| format!("creating {}", cmd_dir.display()))?;
    for (file, content) in BUNDLED_COMMANDS {
        let path = cmd_dir.join(file);
        std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Remove the bundled command files (best-effort).
fn remove_commands(config_dir: &Path) {
    let cmd_dir = config_dir.join("commands");
    for (file, _) in BUNDLED_COMMANDS {
        let _ = std::fs::remove_file(cmd_dir.join(file));
    }
}

/// Remove only lens's hook entries. Returns how many groups were removed.
/// (claude; host-aware is uninstall_for)
pub fn uninstall(settings: &Path) -> Result<usize> {
    uninstall_for(Host::Claude, settings)
}

fn uninstall_for(host: Host, target: &Path) -> Result<usize> {
    match host {
        Host::Claude => uninstall_claude(target),
        Host::Opencode => uninstall_opencode(target),
    }
}

fn uninstall_claude(settings: &Path) -> Result<usize> {
    let mut root = load(settings)?;
    let removed = strip_lens(&mut root);
    save(settings, &root)?;
    if let Some(p) = settings.parent() {
        remove_commands(p);
    }
    Ok(removed)
}

fn uninstall_opencode(config_dir: &Path) -> Result<usize> {
    remove_commands(config_dir);
    let _ = std::fs::remove_file(config_dir.join("plugins").join(OPENCODE_PLUGIN_FILE));
    // Also drop the mcp.lens entry setup wrote, so opencode uninstall is as
    // complete as the Claude path's `claude mcp remove lens`.
    match crate::setup::unregister_mcp_opencode() {
        Ok(true) => Ok(1),
        _ => Ok(0),
    }
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
        .map(|c| c.contains(SELF_MARKER) && (c.contains(MARKER) || c.contains(OPENCODE_MARKER)))
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

/// Remove Context Mode's wiring from `settings` so lens's hooks can install without
/// double-firing on the same lifecycle events: drops any `enabledPlugins` /
/// `enabled_plugins` entry whose key starts with `context-mode`, and any hook group
/// whose command mentions `context-mode`. Idempotent (a no-op when absent). Returns
/// how many entries were removed (plugin keys + hook groups). After this,
/// [`context_mode_present`] reads false, so [`install`] no longer refuses.
pub fn purge_context_mode(settings: &Path) -> Result<usize> {
    let mut root = load(settings)?;
    let removed = strip_context_mode(&mut root);
    if removed > 0 {
        save(settings, &root)?;
    }
    Ok(removed)
}

/// Strip Context Mode plugin entries + hook groups from `root` in place, pruning
/// emptied hook events. Returns the count removed. See [`purge_context_mode`].
fn strip_context_mode(root: &mut Value) -> usize {
    let mut removed = 0;
    for key in ["enabledPlugins", "enabled_plugins"] {
        if let Some(map) = root.get_mut(key).and_then(|v| v.as_object_mut()) {
            let keys: Vec<String> = map
                .keys()
                .filter(|k| k.starts_with("context-mode"))
                .cloned()
                .collect();
            for k in keys {
                map.remove(&k);
                removed += 1;
            }
        }
    }
    if let Some(hooks) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        let mut empty_events = Vec::new();
        for (event, groups) in hooks.iter_mut() {
            if let Some(arr) = groups.as_array_mut() {
                let before = arr.len();
                arr.retain(|g| !group_mentions_context_mode(g));
                removed += before - arr.len();
                if arr.is_empty() {
                    empty_events.push(event.clone());
                }
            }
        }
        for e in empty_events {
            hooks.remove(&e);
        }
    }
    removed
}

fn group_mentions_context_mode(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(|h| h.as_array())
        .map(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(|c| c.as_str())
                    .map(|c| c.contains("context-mode"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
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
    status_for(Host::Claude, settings)
}

fn status_for(host: Host, target: &Path) -> Status {
    match host {
        Host::Claude => status_claude(target),
        Host::Opencode => status_opencode(target),
    }
}

fn status_claude(settings: &Path) -> Status {
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

fn status_opencode(config_dir: &Path) -> Status {
    // Opencode: report commands + plugin presence via the events vec for status.
    // (avoids changing pub Status struct)
    let cmd_dir = config_dir.join("commands");
    let mut installed_events = Vec::new();
    if cmd_dir.join("dashboard.md").is_file() {
        installed_events.push("dashboard".to_string());
    }
    if cmd_dir.join("warmup.md").is_file() {
        installed_events.push("warmup".to_string());
    }
    if !installed_events.is_empty() {
        installed_events.insert(0, "commands".to_string());
    }
    if config_dir.join("plugins").join(OPENCODE_PLUGIN_FILE).is_file() {
        installed_events.push("plugin".to_string());
    }

    let project = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let data_dir = super::resolve_data_dir(&project);
    let store_ok = SessionStore::open(&data_dir).is_ok();
    let fts_ok = Index::open(&data_dir).and_then(|i| i.chunk_count()).is_ok();

    Status {
        installed_events,
        conflict: false,
        store_ok,
        fts_ok,
    }
}

fn print_status_for(host: Host, s: &Status) {
    let mark = |b: bool| if b { "ok" } else { "FAIL" };
    println!("lens session status");
    if host == Host::Claude {
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
    } else {
        // opencode
        if s.installed_events.is_empty() {
            println!("  commands installed : none (run `lens session install --client opencode`)");
        } else {
            println!(
                "  commands installed : {}",
                s.installed_events.join(", ")
            );
        }
        if s.installed_events.iter().any(|e| e == "plugin") {
            println!("  lifecycle hooks : via plugins/lens.js (tool before/after, prompt, session events)");
        } else {
            println!("  lifecycle hooks : plugin not installed (run `lens session install --client opencode`)");
        }
    }
    println!("  event store     : {}", mark(s.store_ok));
    println!("  Search index    : {}", mark(s.fts_ok));
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
            s(&["install", "--config-dir", "/x/cfg"]),
            Some(PathBuf::from("/x/cfg/settings.json"))
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

    #[test]
    fn purge_context_mode_clears_plugin_and_hooks_then_install_succeeds() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        write(
            &settings,
            &json!({
                "enabledPlugins": { "context-mode@context-mode": true, "other@x": true },
                "hooks": {
                    "SessionStart": [
                        { "matcher": "", "hooks": [ { "type": "command", "command": "context-mode hook start" } ] }
                    ]
                }
            }),
        );
        let removed = purge_context_mode(&settings).unwrap();
        assert_eq!(removed, 2); // one plugin key + one hook group
        let root = load(&settings).unwrap();
        assert!(!context_mode_present(&root));
        assert_eq!(root["enabledPlugins"]["other@x"], true); // unrelated plugin kept
        // The emptied SessionStart event was pruned.
        assert!(root["hooks"].get("SessionStart").is_none());
        // install no longer refuses.
        assert!(install(&settings, "/usr/bin/lens").is_ok());
    }

    #[test]
    fn purge_context_mode_is_noop_when_absent() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        write(&settings, &json!({ "enabledPlugins": { "other@x": true } }));
        assert_eq!(purge_context_mode(&settings).unwrap(), 0);
    }

    #[test]
    fn install_writes_dashboard_command_and_uninstall_removes_it() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        install(&settings, "/usr/bin/lens").unwrap();
        let cmd = dir.path().join("commands").join("dashboard.md");
        assert!(cmd.is_file(), "/dashboard command should be installed");
        let body = std::fs::read_to_string(&cmd).unwrap();
        assert!(body.contains("Launch the lens live dashboard"));
        assert!(body.contains("lens dashboard --port"));
        uninstall(&settings).unwrap();
        assert!(!cmd.exists(), "/dashboard command should be removed on uninstall");
    }

    #[test]
    fn install_writes_warmup_command_and_uninstall_removes_it() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        install(&settings, "/usr/bin/lens").unwrap();
        let cmd = dir.path().join("commands").join("warmup.md");
        assert!(cmd.is_file(), "/warmup command should be installed");
        let body = std::fs::read_to_string(&cmd).unwrap();
        assert!(body.contains("lens warmup"));
        uninstall(&settings).unwrap();
        assert!(!cmd.exists(), "/warmup command should be removed on uninstall");
    }

    #[test]
    fn install_opencode_via_run_cli_writes_commands_cross_host() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("oc-cmds");
        let prev_oc = std::env::var_os("OPENCODE_CONFIG_DIR");
        let prev_host = std::env::var_os("LENS_HOST");
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg.to_str().unwrap());
        std::env::set_var("LENS_HOST", "opencode");
        let res = run_cli(&["install".to_string()]);
        assert!(res.is_ok());
        assert!(cfg.join("commands").join("dashboard.md").is_file());
        assert!(cfg.join("commands").join("warmup.md").is_file());
        // the lifecycle plugin is written with the placeholders filled
        let plugin = cfg.join("plugins").join("lens.js");
        assert!(plugin.is_file(), "plugins/lens.js should be installed");
        let body = std::fs::read_to_string(&plugin).unwrap();
        assert!(!body.contains("__LENS_BIN__"), "bin placeholder must be filled");
        assert!(!body.contains("__LENS_ROUTING__"), "routing placeholder must be filled");
        assert!(body.contains("hook\", \"opencode\"") || body.contains("\"hook\", \"opencode\""));
        // no mcp entry registered in this fixture, so routing defaults to full
        assert!(body.contains("LENS_ROUTING = \"full\""));
        // cross-host: flip to claude (no claude writes here), ensure opencode commands untouched
        std::env::set_var("LENS_HOST", "claude");
        assert!(cfg.join("commands").join("dashboard.md").is_file());
        // uninstall removes commands + plugin
        std::env::set_var("LENS_HOST", "opencode");
        run_cli(&["uninstall".to_string()]).unwrap();
        assert!(!plugin.exists(), "plugin removed on uninstall");
        assert!(!cfg.join("commands").join("dashboard.md").exists());
        restore_env("OPENCODE_CONFIG_DIR", prev_oc);
        restore_env("LENS_HOST", prev_host);
    }

    fn restore_env(k: &str, v: Option<std::ffi::OsString>) {
        match v {
            Some(val) => std::env::set_var(k, val),
            None => std::env::remove_var(k),
        }
    }
}
