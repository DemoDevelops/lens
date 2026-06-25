//! `lens setup [--full] [--routing LEVEL] [--config-dir DIR] [--bin-dir DIR]`
//! — one self-contained command that a freshly-downloaded `lens` binary runs to
//! install itself for the current user.
//!
//! It does what the `install.sh` / `setup.sh` scripts do, but from inside the
//! binary, so distributing lens collapses to "send the binary, run `./lens setup`":
//! copy self onto PATH, register the MCP server, install the session hooks
//! (auto-removing a conflicting Context Mode), install + dedup the RTK hook, set the
//! routing level, then print a verification report.
//!
//! A separate process from the MCP server — its stdout is its own response channel,
//! never the JSON-RPC stream.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::rtk;
use crate::session;

/// Routing levels accepted by `--routing` (mirrors `routing::Level::parse`).
const ROUTING_LEVELS: [&str; 5] = ["off", "nudge", "steer", "wrap", "full"];

/// Default release repo for `lens update` (override with `$LENS_REPO`). Matches
/// `install.sh`'s `REPO`.
const DEFAULT_REPO: &str = "DemoDevelops/lens";

/// Parsed `lens setup` options.
struct Opts {
    routing: String,
    bin_dir: PathBuf,
    config_dir: Option<PathBuf>,
}

/// CLI entry: `args` is everything after `setup`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let opts = parse_opts(args)?;

    // Make every downstream installer agree on which Claude config dir to write:
    // `claude_settings_path()`, `claude mcp add`, and the hook installers all read
    // `$CLAUDE_CONFIG_DIR`, so set it once up front when targeting a specific account.
    if let Some(dir) = &opts.config_dir {
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
    }

    if !cmd_exists("claude") {
        bail!("Claude Code ('claude') not found on PATH. Install it first: https://claude.com/claude-code");
    }

    let settings = rtk::claude_settings_path()
        .ok_or_else(|| anyhow!("cannot resolve Claude settings path (is $HOME set?)"))?;

    // 1. Copy this binary to a stable location; use THAT path everywhere so the
    //    MCP server + hooks keep working after the downloaded copy is deleted.
    let bin = install_self(&opts.bin_dir).context("installing the lens binary")?;
    say(&format!("Installed binary: {}", bin.display()));

    // 2. Register the MCP server (the lens_* tools).
    match register_mcp(&bin) {
        Ok(true) => say("Registered MCP server 'lens'."),
        Ok(false) => say("MCP server 'lens' already registered."),
        Err(e) => warn(&format!(
            "could not register MCP server: {e:#}\n  register by hand: claude mcp add lens --scope user -- {}",
            bin.display()
        )),
    }

    // 3. Session hooks — clear a conflicting Context Mode first (install refuses
    //    to coexist with it), then install lens's five lifecycle hooks.
    match session::install::purge_context_mode(&settings) {
        Ok(n) if n > 0 => say(&format!(
            "Removed Context Mode wiring ({n} entr{}).",
            if n == 1 { "y" } else { "ies" }
        )),
        Ok(_) => {}
        Err(e) => warn(&format!("could not check for Context Mode: {e:#}")),
    }
    let bin_str = bin.to_string_lossy().to_string();
    session::install::install(&settings, &bin_str).context("installing session hooks")?;
    say("Installed session hooks (5 lifecycle events).");

    // 4. RTK shell compression — install, then dedup to exactly one rtk hook so a
    //    pre-existing rtk install can't double-fire alongside lens's managed one.
    match rtk::install::install() {
        Ok(()) => {
            match rtk::install::dedup_rtk_hooks(&settings) {
                Ok(n) if n > 0 => say(&format!(
                    "Deduplicated RTK hooks (removed {n} extra so exactly one remains)."
                )),
                Ok(_) => {}
                Err(e) => warn(&format!("could not dedup RTK hooks: {e:#}")),
            }
            say("Installed RTK shell compression.");
        }
        Err(e) => warn(&format!(
            "RTK install skipped (non-fatal): {e:#}\n  retry later with: lens rtk install"
        )),
    }

    // 5. Routing level.
    set_routing(&settings, &opts.routing).context("setting routing level")?;
    say(&format!("Set routing level: {}", opts.routing));

    // 6. PATH — so `lens` works as a bare command in new shells.
    let path_added = ensure_on_path(&opts.bin_dir);

    // 7. Verify and report.
    println!();
    let ok = doctor(&settings, &bin, &opts.bin_dir, path_added);
    println!();
    if ok {
        println!("Done. Restart Claude Code to load lens (verify with the lens_stats tool).");
    } else {
        println!("Setup finished with the warnings above. Restart Claude Code, then fix the flagged items or re-run `lens setup`.");
    }
    println!(
        "Uninstall: lens session uninstall && lens rtk uninstall && claude mcp remove lens && rm {}",
        bin.display()
    );
    Ok(())
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut routing: Option<String> = None;
    let mut full = false;
    let mut bin_dir: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--full" => full = true,
            "--routing" => {
                routing = Some(args.get(i + 1).context("--routing needs a value")?.clone());
                i += 1;
            }
            "--bin-dir" => {
                bin_dir = Some(PathBuf::from(args.get(i + 1).context("--bin-dir needs a value")?));
                i += 1;
            }
            "--config-dir" => {
                config_dir =
                    Some(PathBuf::from(args.get(i + 1).context("--config-dir needs a value")?));
                i += 1;
            }
            other => bail!("lens setup: unknown option '{other}'"),
        }
        i += 1;
    }

    let routing = resolve_routing(routing.as_deref(), full)?;
    let bin_dir = match bin_dir.or_else(|| std::env::var_os("LENS_BIN_DIR").map(PathBuf::from)) {
        Some(d) => d,
        None => default_bin_dir()?,
    };
    Ok(Opts {
        routing,
        bin_dir,
        config_dir,
    })
}

/// Resolve the routing level: explicit `--routing` wins, else `full` when `--full`
/// is set, else the safe `nudge` default. Rejects an unknown level.
fn resolve_routing(explicit: Option<&str>, full: bool) -> Result<String> {
    let level = explicit
        .map(|s| s.to_string())
        .unwrap_or_else(|| if full { "full".into() } else { "nudge".into() });
    if !ROUTING_LEVELS.contains(&level.as_str()) {
        bail!(
            "invalid routing level '{level}' (use one of: {})",
            ROUTING_LEVELS.join(", ")
        );
    }
    Ok(level)
}

/// Default install dir for the binary: `~/.local/bin` (matches `install.sh`).
fn default_bin_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|s| !s.is_empty())
        .context("HOME not set")?;
    Ok(PathBuf::from(home).join(".local").join("bin"))
}

/// Copy the running binary into `bin_dir` as `lens`, executable. Skips the copy when
/// already running from the target. Copies via a temp file + atomic rename so a
/// running server's mapped binary is never truncated. Returns the installed path.
fn install_self(bin_dir: &Path) -> Result<PathBuf> {
    let src = std::env::current_exe().context("resolving current executable")?;
    let name = if cfg!(windows) { "lens.exe" } else { "lens" };
    let dst = bin_dir.join(name);

    std::fs::create_dir_all(bin_dir).with_context(|| format!("creating {}", bin_dir.display()))?;

    // Already running from the install target? Nothing to copy.
    if dst.exists() && std::fs::canonicalize(&src).ok() == std::fs::canonicalize(&dst).ok() {
        return Ok(dst);
    }

    let tmp = bin_dir.join(".lens.download");
    std::fs::copy(&src, &tmp)
        .with_context(|| format!("copying {} -> {}", src.display(), tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok();
    }
    std::fs::rename(&tmp, &dst).with_context(|| format!("installing {}", dst.display()))?;

    // curl/scp downloads aren't quarantined like browser ones, but strip it anyway
    // so a re-copy can never trip Gatekeeper.
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&dst)
            .output();
    }
    Ok(dst)
}

/// `claude mcp add lens --scope user -- <bin>`. `Ok(true)` if newly added, `Ok(false)`
/// if it was already registered, `Err` if `claude` couldn't run at all.
fn register_mcp(bin: &Path) -> Result<bool> {
    let out = Command::new("claude")
        .args(["mcp", "add", "lens", "--scope", "user", "--"])
        .arg(bin)
        .output()
        .context("running `claude mcp add`")?;
    if out.status.success() {
        return Ok(true);
    }
    // Non-zero is usually "already exists" — treat a present registration as success.
    if mcp_registered() {
        return Ok(false);
    }
    bail!(
        "`claude mcp add` failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
}

/// Does `claude mcp list` show a `lens` server?
fn mcp_registered() -> bool {
    Command::new("claude")
        .args(["mcp", "list"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim_start().starts_with("lens"))
        })
        .unwrap_or(false)
}

/// Write `env.LENS_ROUTING = level` into `settings`, preserving everything else.
fn set_routing(settings: &Path, level: &str) -> Result<()> {
    let mut root = read_json(settings)?;
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let env = obj.entry("env").or_insert_with(|| json!({}));
    if !env.is_object() {
        *env = json!({});
    }
    env.as_object_mut()
        .unwrap()
        .insert("LENS_ROUTING".to_string(), json!(level));
    write_json(settings, &root)
}

/// Append `<bin_dir>` to the user's shell profile if it isn't already on PATH.
/// Returns true if a profile was modified (caller tells the user to open a new shell).
fn ensure_on_path(bin_dir: &Path) -> bool {
    if dir_on_path(bin_dir) {
        return false;
    }
    let Some(profile) = shell_profile() else {
        return false;
    };
    let needle = bin_dir.display().to_string();
    if let Ok(existing) = std::fs::read_to_string(&profile) {
        if existing.contains(&needle) {
            return false;
        }
    }
    let block = format!("\n# added by lens setup\nexport PATH=\"{needle}:$PATH\"\n");
    use std::io::Write;
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&profile)
    {
        Ok(mut f) => f.write_all(block.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// Is `dir` an entry in `$PATH`?
fn dir_on_path(dir: &Path) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|p| p == dir)
}

/// The shell profile to append a PATH line to, by `$SHELL`: zsh→`.zshrc`,
/// bash→`.bashrc`, else `.profile`. `None` if `$HOME` is unset.
fn shell_profile() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let shell = std::env::var("SHELL").unwrap_or_default();
    let base = Path::new(&shell)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    let file = match base {
        "zsh" => ".zshrc",
        "bash" => ".bashrc",
        _ => ".profile",
    };
    Some(home.join(file))
}

/// Print the install verification (mirrors the checklist a hand-written install
/// prompt would run) and return whether every check passed.
fn doctor(settings: &Path, bin: &Path, bin_dir: &Path, path_added: bool) -> bool {
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    checks.push(("MCP server registered".into(), mcp_registered(), String::new()));

    let st = session::install::status(settings);
    checks.push((
        "session hooks installed".into(),
        st.installed_events.len() == 5,
        format!("{}/5", st.installed_events.len()),
    ));
    checks.push(("Context Mode not present".into(), !st.conflict, String::new()));

    let n = rtk::install::count_rtk_hooks(settings);
    checks.push(("exactly one RTK hook".into(), n == 1, format!("{n} found")));

    let cmd_present = settings
        .parent()
        .map(|d| d.join("commands").join("dashboard.md").is_file())
        .unwrap_or(false);
    checks.push(("/dashboard command installed".into(), cmd_present, String::new()));

    let on_path_now = cmd_exists("lens");
    let path_ok = bin.is_file() && (on_path_now || dir_on_path(bin_dir) || path_added);
    let note = if on_path_now || dir_on_path(bin_dir) {
        String::new()
    } else if path_added {
        format!("{} added to your profile — open a new terminal", bin_dir.display())
    } else {
        format!("add {} to your PATH", bin_dir.display())
    };
    checks.push(("lens resolves on PATH".into(), path_ok, note));

    println!("Verifying:");
    let mut all_ok = true;
    for (label, ok, note) in &checks {
        all_ok &= *ok;
        let mark = if *ok { "ok  " } else { "FAIL" };
        if note.is_empty() {
            println!("  [{mark}] {label}");
        } else {
            println!("  [{mark}] {label} — {note}");
        }
    }
    all_ok
}

/// Does `name` resolve on PATH? (`command -v`, matching the rtk hook's own probe.)
fn cmd_exists(name: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {name}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn read_json(path: &Path) -> Result<Value> {
    if !path.is_file() {
        return Ok(json!({}));
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, serde_json::to_string_pretty(v)? + "\n")
        .with_context(|| format!("writing {}", path.display()))
}

fn say(msg: &str) {
    println!("==> {msg}");
}

fn warn(msg: &str) {
    eprintln!("warning: {msg}");
}

// ── `lens update` ───────────────────────────────────────────────────────────

/// CLI entry for `lens update`: if a newer release exists, download the matching
/// binary and re-run `setup` with it (preserving routing level + install location).
/// Needs `gh` (the repo is private); falls back to a clear message otherwise.
pub fn run_update_cli(args: &[String]) -> Result<()> {
    let config_dir = parse_config_dir(args);
    if let Some(dir) = &config_dir {
        std::env::set_var("CLAUDE_CONFIG_DIR", dir);
    }

    if !cmd_exists("gh") {
        bail!("`lens update` needs the GitHub CLI to reach the private repo. Install gh (https://cli.github.com), run `gh auth login`, then retry — or re-run `lens setup` with a binary you were sent.");
    }

    let repo = repo();
    let current = env!("CARGO_PKG_VERSION");
    let tag = latest_tag(&repo)?;
    if !is_newer(&tag, current) {
        println!("lens is up to date (v{current}; latest release is {tag}).");
        return Ok(());
    }
    say(&format!("Updating lens v{current} -> {tag}..."));

    let target = lens_target()?;
    let tmp = std::env::temp_dir().join(format!("lens-{target}.update"));
    download_release(&repo, &tag, target, &tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).ok();
    }
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("xattr")
            .args(["-d", "com.apple.quarantine"])
            .arg(&tmp)
            .output();
    }

    // Re-apply install with the NEW binary so it copies itself onto PATH and
    // refreshes the hooks + /dashboard command. Preserve the current routing level
    // and the existing install location.
    let settings = rtk::claude_settings_path()
        .ok_or_else(|| anyhow!("cannot resolve Claude settings path"))?;
    let routing = current_routing(&settings).unwrap_or_else(|| "nudge".to_string());

    let mut cmd = Command::new(&tmp);
    cmd.arg("setup").arg("--routing").arg(&routing);
    if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
        cmd.arg("--bin-dir").arg(dir);
    }
    if let Some(dir) = &config_dir {
        cmd.arg("--config-dir").arg(dir);
    }
    let status = cmd.status().context("running the new binary's `setup`")?;
    let _ = std::fs::remove_file(&tmp);
    if !status.success() {
        bail!("the new binary's `setup` step failed (see output above)");
    }
    Ok(())
}

/// Release repo slug: `$LENS_REPO` or [`DEFAULT_REPO`].
fn repo() -> String {
    std::env::var("LENS_REPO")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// The release asset target for this host (matches `.github/workflows/release.yml`).
fn lens_target() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        (os, arch) => bail!("no prebuilt lens binary for {os}/{arch}; build + `setup` from source"),
    })
}

/// Latest published release tag via `gh release view`.
fn latest_tag(repo: &str) -> Result<String> {
    let out = Command::new("gh")
        .args([
            "release", "view", "--repo", repo, "--json", "tagName", "--jq", ".tagName",
        ])
        .output()
        .context("running `gh release view` (is gh authenticated?)")?;
    if !out.status.success() {
        bail!(
            "`gh release view` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if tag.is_empty() {
        bail!("no releases found for {repo}");
    }
    Ok(tag)
}

/// Download `lens-<target>` from `tag` to `dest` via `gh release download`.
fn download_release(repo: &str, tag: &str, target: &str, dest: &Path) -> Result<()> {
    let out = Command::new("gh")
        .args([
            "release",
            "download",
            tag,
            "--repo",
            repo,
            "--pattern",
            &format!("lens-{target}"),
            "--clobber",
            "--output",
        ])
        .arg(dest)
        .output()
        .context("running `gh release download`")?;
    if !out.status.success() {
        bail!(
            "downloading lens-{target} from {tag} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// The routing level currently recorded in `settings` (`env.LENS_ROUTING`), if any.
fn current_routing(settings: &Path) -> Option<String> {
    read_json(settings)
        .ok()?
        .get("env")?
        .get("LENS_ROUTING")?
        .as_str()
        .map(|s| s.to_string())
}

/// Extract `--config-dir <dir>` from args, if present.
fn parse_config_dir(args: &[String]) -> Option<PathBuf> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--config-dir" {
            return args.get(i + 1).map(PathBuf::from);
        }
        i += 1;
    }
    None
}

/// Parse `x.y.z` (ignoring any `-rc`/`+build` suffix and a leading `v`) into a
/// comparable tuple. `None` if it isn't three numeric components.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next().unwrap_or(core);
    let mut parts = core.split('.');
    let a = parts.next()?.parse().ok()?;
    let b = parts.next()?.parse().ok()?;
    let c = parts.next()?.parse().ok()?;
    Some((a, b, c))
}

/// Is `latest` a newer version than `current`? Unparseable input reads as not-newer,
/// so a malformed tag never triggers an automatic binary replacement.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_routing_defaults_and_validates() {
        assert_eq!(resolve_routing(None, false).unwrap(), "nudge");
        assert_eq!(resolve_routing(None, true).unwrap(), "full");
        // Explicit wins over --full.
        assert_eq!(resolve_routing(Some("wrap"), true).unwrap(), "wrap");
        assert_eq!(resolve_routing(Some("off"), false).unwrap(), "off");
        // Unknown level is rejected.
        assert!(resolve_routing(Some("loud"), false).is_err());
    }

    #[test]
    fn set_routing_writes_env_and_preserves_other_keys() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({
                "env": { "EXISTING": "1" },
                "hooks": { "PreToolUse": [] }
            }))
            .unwrap(),
        )
        .unwrap();

        set_routing(&settings, "full").unwrap();

        let root = read_json(&settings).unwrap();
        assert_eq!(root["env"]["LENS_ROUTING"], "full");
        assert_eq!(root["env"]["EXISTING"], "1"); // preserved
        assert!(root["hooks"].is_object()); // preserved
    }

    #[test]
    fn set_routing_creates_missing_file() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("nested").join("settings.json");
        set_routing(&settings, "nudge").unwrap();
        let root = read_json(&settings).unwrap();
        assert_eq!(root["env"]["LENS_ROUTING"], "nudge");
    }

    #[test]
    fn dir_on_path_detects_membership() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let target = dir.path().join("bin");
        std::env::set_var("PATH", format!("/usr/bin:{}", target.display()));
        assert!(dir_on_path(&target));
        std::env::set_var("PATH", "/usr/bin:/bin");
        assert!(!dir_on_path(&target));
    }

    #[test]
    fn shell_profile_maps_known_shells() {
        let _g = crate::rtk::env_test_lock();
        std::env::set_var("HOME", "/home/x");
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(shell_profile().unwrap(), PathBuf::from("/home/x/.zshrc"));
        std::env::set_var("SHELL", "/usr/bin/bash");
        assert_eq!(shell_profile().unwrap(), PathBuf::from("/home/x/.bashrc"));
        std::env::set_var("SHELL", "/usr/bin/fish");
        assert_eq!(shell_profile().unwrap(), PathBuf::from("/home/x/.profile"));
        std::env::remove_var("SHELL");
        std::env::remove_var("HOME");
    }

    #[test]
    fn version_compare_is_numeric_not_lexical() {
        assert_eq!(parse_version("v1.2.3-rc1"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1.2"), Some((0, 1, 2)));
        assert_eq!(parse_version("garbage"), None);
        assert!(is_newer("v0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.2")); // numeric, not string, compare
        assert!(!is_newer("0.1.2", "0.1.2")); // equal
        assert!(!is_newer("v0.1.1", "0.1.2")); // older
        assert!(!is_newer("garbage", "0.1.2")); // unparseable never updates
    }
}
