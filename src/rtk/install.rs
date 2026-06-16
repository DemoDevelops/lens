//! `ctxforge rtk install | status | uninstall` — manage the prebuilt RTK binary
//! and its Claude Code hook (the **headroom pattern**; see `RTK_NOTES.md` §1/§5/§6).
//!
//! [`install`] detects the platform target triple, downloads the pinned release
//! `rtk-<triple>.<ext>` with `curl`, extracts it (`tar`/`unzip`) into
//! `~/.ctxforge/bin/rtk`, `chmod +x`'s it, verifies `rtk --version`, then registers
//! the Claude hook by shelling out to `rtk init --global --hook-only --auto-patch`
//! (headroom registers via `rtk init --global --auto-patch`; we add `--hook-only`
//! so only the PreToolUse hook is written, not RTK.md / a CLAUDE.md edit). [`uninstall`]
//! runs `rtk init --global --uninstall`; [`status`] reports the resolved binary,
//! version, hook registration, and a one-line gain summary.
//!
//! Everything shells out via `std::process::Command` (`curl`/`tar`/`unzip`/`chmod`) —
//! no extra crate dependency. The network download path is verified on-machine only
//! (an isolated `$HOME`), never in CI; see the module's offline `target_triple`/
//! `download_url` unit tests.
//!
//! This is reached only through the `ctxforge rtk …` subcommand — a separate
//! process whose stdout is its own response channel, never the MCP JSON-RPC stream.

use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};

use super::gain::{self, Scope};

/// GitHub releases base — `…/download/<ver>/rtk-<triple>.<ext>` (mirrors
/// headroom `installer.py::GITHUB_RELEASE_URL`).
const GITHUB_RELEASE_URL: &str = "https://github.com/rtk-ai/rtk/releases/download";

/// The release target triple to install — `$CTXFORGE_RTK_TARGET` if set, else
/// detected from the build's `OS`/`ARCH` (mirrors headroom
/// `_detect_runtime_target_triple`; override env is `HEADROOM_RTK_TARGET` there).
///
/// Returns `Err` only for an unsupported platform.
fn target_triple() -> Result<String> {
    if let Some(t) = std::env::var_os("CTXFORGE_RTK_TARGET") {
        let t = t.to_string_lossy();
        let t = t.trim();
        if !t.is_empty() {
            return Ok(t.to_string());
        }
    }
    let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", _) => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", _) => "x86_64-unknown-linux-musl",
        ("windows", _) => "x86_64-pc-windows-msvc",
        (os, arch) => bail!("unsupported platform for rtk: {os}/{arch}"),
    };
    Ok(triple.to_string())
}

/// Archive extension for a target triple — `zip` for Windows, else `tar.gz`.
fn archive_ext(triple: &str) -> &'static str {
    if triple.contains("windows") {
        "zip"
    } else {
        "tar.gz"
    }
}

/// Build the release asset URL for `version` (e.g. `v0.28.2`) and `triple`.
fn download_url(version: &str, triple: &str) -> String {
    format!(
        "{GITHUB_RELEASE_URL}/{version}/rtk-{triple}.{ext}",
        ext = archive_ext(triple)
    )
}

/// The version to install — `$CTXFORGE_RTK_VERSION` if set, else the pinned
/// [`super::RTK_VERSION`] (`v0.28.2`).
fn version() -> String {
    std::env::var("CTXFORGE_RTK_VERSION")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| super::RTK_VERSION.to_string())
}

/// The bare version digits to look for in `rtk --version` output (strip a leading
/// `v`): the binary prints `rtk 0.28.2`, but our pin is `v0.28.2`. See RTK_NOTES §3.
fn version_digits(version: &str) -> &str {
    version.strip_prefix('v').unwrap_or(version)
}

/// Run `<bin> --version` and return its trimmed stdout, or `Err` if it can't be
/// spawned or exits nonzero.
fn run_version(bin: &Path) -> Result<String> {
    let out = Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("failed to run {} --version", bin.display()))?;
    if !out.status.success() {
        bail!(
            "{} --version exited {}: {}",
            bin.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Download + install the pinned RTK binary and register its Claude hook.
///
/// Idempotent: if the managed binary already reports the expected version the
/// download is skipped and only the hook is (re-)registered (`rtk init` is itself
/// idempotent). A failed hook registration is a **warning**, not a fatal error —
/// the binary is still installed and usable.
pub fn install() -> Result<()> {
    let version = version();
    let triple = target_triple()?;
    let want_digits = version_digits(&version);

    let bin_dir = super::bin_dir().context("cannot determine ctxforge home (is $HOME set?)")?;
    let managed = bin_dir.join(super::RTK_EXE);

    // --- Idempotency: skip download if the managed binary already matches. ---
    let already = managed.is_file()
        && run_version(&managed)
            .ok()
            .is_some_and(|v| v.contains(want_digits));

    if already {
        println!(
            "rtk {version} already installed at {} — skipping download.",
            managed.display()
        );
    } else {
        download_and_extract(&version, &triple, &bin_dir, &managed)?;
        let reported = run_version(&managed)
            .with_context(|| format!("verifying {}", managed.display()))?;
        if !reported.contains(want_digits) {
            bail!(
                "rtk verification failed: expected version containing `{want_digits}`, got `{reported}`"
            );
        }
        println!("Installed {reported} at {}", managed.display());
    }

    // --- Register the Claude hook in the dir THIS Claude Code reads. ---
    if let Err(e) = register_hook() {
        eprintln!("warning: could not register RTK hook: {e:#}");
    } else if super::rtk_hook_registered() {
        match super::claude_settings_path() {
            Some(p) => println!("RTK hook registered in {}", p.display()),
            None => println!("RTK hook registered in Claude settings."),
        }
    }
    Ok(())
}

/// Register the RTK PreToolUse hook in the active Claude config dir's settings.json.
///
/// `rtk init --global` ignores `$CLAUDE_CONFIG_DIR` (it always writes `~/.claude`),
/// so ctxforge owns the settings patch: generate the hook SCRIPT via `rtk init
/// --hook-only --no-patch`, then patch [`claude_settings_path`] ourselves — copying
/// the script into that dir's `hooks/` when it differs from rtk's default `~/.claude`,
/// so the hook is self-contained under the dir the running Claude Code actually reads.
/// `--hook-only` writes only the script (no RTK.md / CLAUDE.md edits); ctxforge owns
/// the model-facing guidance.
fn register_hook() -> Result<()> {
    // 1. Generate the hook script (no settings patch — ctxforge owns that).
    match super::run_rtk(&["init", "--global", "--hook-only", "--no-patch"]) {
        Ok(out) if out.status.success() => {}
        Ok(out) => eprintln!(
            "warning: `rtk init --hook-only --no-patch` exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => return Err(e),
    }

    let settings = super::claude_settings_path().context("cannot resolve Claude settings path")?;
    let settings_dir = settings.parent().context("settings path has no parent")?;
    let canonical =
        super::rtk_default_hook_script().context("cannot resolve rtk hook script path")?;

    // 2. Resolve the script path the entry will point at — inside the config dir.
    let target_script = settings_dir.join("hooks").join("rtk-rewrite.sh");
    let command_path = if canonical == target_script {
        canonical // rtk's default dir IS the config dir — use the script in place.
    } else if canonical.is_file() {
        if let Some(parent) = target_script.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        std::fs::copy(&canonical, &target_script)
            .with_context(|| format!("copying hook script to {}", target_script.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&target_script, std::fs::Permissions::from_mode(0o755));
        }
        target_script
    } else {
        canonical // no script produced (e.g. a stub rtk in tests) — register the path anyway.
    };

    // 3. Idempotently patch the settings.json with the PreToolUse Bash entry.
    let cmd = command_path.to_str().context("hook script path is not UTF-8")?;
    ensure_hook_entry(&settings, cmd).with_context(|| format!("patching {}", settings.display()))
}

/// Idempotently ensure `settings_path` has a PreToolUse Bash hook running `command`.
/// Creates the file/parents if missing; backs up to `*.json.bak` before writing.
/// No-op (no write) if the exact command is already present.
fn ensure_hook_entry(settings_path: &Path, command: &str) -> Result<()> {
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut root = read_settings(settings_path)?;
    if hook_command_present(&root, command) {
        return Ok(());
    }
    let obj = root.as_object_mut().context("settings.json is not a JSON object")?;
    let hooks = obj.entry("hooks").or_insert_with(|| serde_json::json!({}));
    let hooks_obj = hooks.as_object_mut().context("`hooks` is not an object")?;
    let pre = hooks_obj
        .entry("PreToolUse")
        .or_insert_with(|| serde_json::json!([]));
    let arr = pre.as_array_mut().context("`hooks.PreToolUse` is not an array")?;
    arr.push(serde_json::json!({
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": command }],
    }));
    write_settings(settings_path, &root)
}

/// Remove every PreToolUse entry whose command contains `needle`. Returns whether
/// anything changed.
fn remove_hook_entry(settings_path: &Path, needle: &str) -> Result<bool> {
    if !settings_path.is_file() {
        return Ok(false);
    }
    let mut root = read_settings(settings_path)?;
    let Some(arr) = root
        .get_mut("hooks")
        .and_then(|h| h.get_mut("PreToolUse"))
        .and_then(|p| p.as_array_mut())
    else {
        return Ok(false);
    };
    let before = arr.len();
    arr.retain(|entry| !entry_command_contains(entry, needle));
    if arr.len() == before {
        return Ok(false);
    }
    write_settings(settings_path, &root)?;
    Ok(true)
}

fn read_settings(path: &Path) -> Result<serde_json::Value> {
    if !path.is_file() {
        return Ok(serde_json::json!({}));
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

fn write_settings(path: &Path, root: &serde_json::Value) -> Result<()> {
    if path.is_file() {
        let _ = std::fs::copy(path, path.with_extension("json.bak"));
    }
    let body = serde_json::to_string_pretty(root)?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

fn hook_command_present(root: &serde_json::Value, command: &str) -> bool {
    root.get("hooks")
        .and_then(|h| h.get("PreToolUse"))
        .and_then(|p| p.as_array())
        .is_some_and(|arr| arr.iter().any(|e| entry_command_eq(e, command)))
}

fn entry_command_eq(entry: &serde_json::Value, command: &str) -> bool {
    entry
        .get("hooks")
        .and_then(|h| h.as_array())
        .is_some_and(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command)))
}

fn entry_command_contains(entry: &serde_json::Value, needle: &str) -> bool {
    entry.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| {
        hs.iter().any(|h| {
            h.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(|c| c.contains(needle))
        })
    })
}

/// Download `rtk-<triple>.<ext>` to a temp file via `curl`, then extract the single
/// top-level `rtk` binary into `bin_dir` and make it executable. The archive layout
/// (one top-level `rtk`) is verified in RTK_NOTES §1, so `tar xzf … -C <bindir>`
/// lands `<bindir>/rtk` directly.
fn download_and_extract(version: &str, triple: &str, bin_dir: &Path, managed: &Path) -> Result<()> {
    let url = download_url(version, triple);
    let ext = archive_ext(triple);

    std::fs::create_dir_all(bin_dir)
        .with_context(|| format!("failed to create {}", bin_dir.display()))?;

    // Temp archive lives in bin_dir so extraction stays on one filesystem.
    let tmp = bin_dir.join(format!("rtk-download.{ext}"));

    println!("Downloading rtk {version} ({triple}) from {url} ...");
    let curl = Command::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tmp)
        .status()
        .context("failed to spawn curl (is it installed?)")?;
    if !curl.success() {
        let _ = std::fs::remove_file(&tmp);
        bail!("curl failed to download {url} (exit {curl})");
    }

    // Extract: the archive holds a single top-level `rtk` (RTK_NOTES §1).
    let extract = if ext == "zip" {
        Command::new("unzip")
            .arg("-o")
            .arg(&tmp)
            .arg("-d")
            .arg(bin_dir)
            .status()
            .context("failed to spawn unzip (is it installed?)")?
    } else {
        Command::new("tar")
            .arg("xzf")
            .arg(&tmp)
            .arg("-C")
            .arg(bin_dir)
            .status()
            .context("failed to spawn tar (is it installed?)")?
    };
    let _ = std::fs::remove_file(&tmp);
    if !extract.success() {
        bail!("failed to extract rtk archive (exit {extract})");
    }

    if !managed.is_file() {
        bail!(
            "rtk binary not found at {} after extracting {url}",
            managed.display()
        );
    }

    // chmod +x (0o755). No-op concept on Windows — guarded out there.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(managed, std::fs::Permissions::from_mode(0o755))
            .with_context(|| format!("failed to chmod +x {}", managed.display()))?;
    }

    Ok(())
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

/// Report install state: binary path, `--version`, hook registration, whether the
/// hook can rewrite live (rtk on PATH + jq), and a one-line gain summary.
/// Best-effort — never errors when RTK is absent.
pub fn status() -> Result<()> {
    match super::rtk_bin_path() {
        Some(bin) => {
            println!("rtk binary: {}", bin.display());
            match run_version(&bin) {
                Ok(v) => println!("version:    {v}"),
                Err(e) => println!("version:    (failed: {e:#})"),
            }
        }
        None => {
            println!("rtk binary: not installed (run `ctxforge rtk install`)");
        }
    }

    println!(
        "hook:       {}",
        if super::rtk_hook_registered() {
            "registered in Claude settings"
        } else {
            "not registered"
        }
    );

    // The registered hook (`rtk-rewrite.sh`) only rewrites commands live if it can
    // find `rtk` on PATH and has `jq` (it shells out to both). We install to
    // `~/.ctxforge/bin`, which isn't on PATH by default, so surface what's needed
    // to actually activate live rewriting (vs. just having the hook registered).
    let on_path = cmd_exists("rtk");
    let jq = cmd_exists("jq");
    if on_path && jq {
        println!("rewrite:    live (rtk on PATH, jq present)");
    } else {
        let mut needs = Vec::new();
        if !on_path {
            match super::bin_dir() {
                Some(b) => needs.push(format!("add {} to PATH", b.display())),
                None => needs.push("put rtk on PATH".to_string()),
            }
        }
        if !jq {
            needs.push("install jq".to_string());
        }
        println!(
            "rewrite:    inactive (hook registered) — to enable live rewriting: {}",
            needs.join("; ")
        );
    }

    let gain_line = match gain::read_gain(Scope::Global) {
        Ok(g) => format!(
            "{} commands, {} tokens saved ({:.1}% avg)",
            g.summary.total_commands, g.summary.total_saved, g.summary.avg_savings_pct
        ),
        Err(_) => "n/a".to_string(),
    };
    println!("gain:       {gain_line}");

    Ok(())
}

/// Remove ctxforge's RTK hook from the active Claude config dir's settings.json and
/// drop the script copy ctxforge placed there. Scoped to ctxforge's own changes: it
/// does **not** run `rtk init --uninstall` (that would also delete a pre-existing
/// `RTK.md` and rtk's own `~/.claude` artifacts). Best-effort; returns `Ok` even when
/// nothing is present. The binary at `~/.ctxforge/bin/rtk` is left in place.
pub fn uninstall() -> Result<()> {
    let settings = super::claude_settings_path().context("cannot resolve Claude settings path")?;
    match remove_hook_entry(&settings, "rtk-rewrite.sh") {
        Ok(true) => println!("Removed RTK hook from {}", settings.display()),
        Ok(false) => println!(
            "no RTK hook found in {} (nothing to remove)",
            settings.display()
        ),
        Err(e) => eprintln!("warning: could not edit {}: {e:#}", settings.display()),
    }
    // Remove the script copy ctxforge placed in a non-default config dir; leave
    // rtk's own ~/.claude/hooks/rtk-rewrite.sh (rtk's artifact) untouched.
    if let (Some(dir), Some(canonical)) = (settings.parent(), super::rtk_default_hook_script()) {
        let copied = dir.join("hooks").join("rtk-rewrite.sh");
        if copied != canonical && copied.is_file() {
            let _ = std::fs::remove_file(&copied);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_url_uses_release_layout_and_ext() {
        assert_eq!(
            download_url("v0.28.2", "aarch64-apple-darwin"),
            "https://github.com/rtk-ai/rtk/releases/download/v0.28.2/rtk-aarch64-apple-darwin.tar.gz"
        );
        // Windows asset is a .zip (RTK_NOTES §1).
        assert_eq!(
            download_url("v0.28.2", "x86_64-pc-windows-msvc"),
            "https://github.com/rtk-ai/rtk/releases/download/v0.28.2/rtk-x86_64-pc-windows-msvc.zip"
        );
    }

    #[test]
    fn archive_ext_is_zip_only_for_windows() {
        assert_eq!(archive_ext("x86_64-pc-windows-msvc"), "zip");
        assert_eq!(archive_ext("aarch64-apple-darwin"), "tar.gz");
        assert_eq!(archive_ext("x86_64-unknown-linux-musl"), "tar.gz");
    }

    #[test]
    fn version_digits_strips_leading_v() {
        // The binary prints `rtk 0.28.2`; our pin is `v0.28.2`.
        assert_eq!(version_digits("v0.28.2"), "0.28.2");
        assert_eq!(version_digits("0.28.2"), "0.28.2");
    }

    #[test]
    fn target_triple_env_override_wins() {
        std::env::set_var("CTXFORGE_RTK_TARGET", "x86_64-unknown-linux-musl");
        assert_eq!(target_triple().unwrap(), "x86_64-unknown-linux-musl");
        std::env::remove_var("CTXFORGE_RTK_TARGET");
        // Unset → a non-empty triple for the current platform (no panic).
        assert!(!target_triple().unwrap().is_empty());
    }
}
