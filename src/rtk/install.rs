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

    // --- Register the Claude hook (`rtk init --global --hook-only --auto-patch`). ---
    // We use `--hook-only` (vs. headroom's full `--auto-patch`): it writes ONLY the
    // PreToolUse hook (`~/.claude/hooks/rtk-rewrite.sh`) + patches settings.json, and
    // does NOT create RTK.md or touch the user's CLAUDE.md. ctxforge owns the
    // model-facing guidance, so injecting RTK's instructions would be redundant.
    // A nonzero exit / spawn failure is a warning, not fatal: the binary is usable
    // and the hook can be (re)registered later via `ctxforge rtk install`.
    match super::run_rtk(&["init", "--global", "--hook-only", "--auto-patch"]) {
        Ok(out) if out.status.success() => {
            println!("Registered RTK Claude hook (rtk init --global --hook-only --auto-patch).");
        }
        Ok(out) => {
            eprintln!(
                "warning: `rtk init --global --hook-only --auto-patch` exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            eprintln!("warning: could not register RTK hook: {e:#}");
        }
    }

    if super::rtk_hook_registered() {
        println!("RTK hook is registered in Claude settings.");
    }
    Ok(())
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

/// Report install state: binary path, `--version`, hook registration, and a
/// one-line gain summary. Best-effort — never errors when RTK is absent.
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

/// Remove RTK's Claude hook (`rtk init --global --uninstall`). Best-effort: the
/// contract is unregistering the hook, not deleting the binary. Returns `Ok` even
/// when RTK is absent.
pub fn uninstall() -> Result<()> {
    match super::run_rtk(&["init", "--global", "--uninstall"]) {
        Ok(out) if out.status.success() => {
            let msg = String::from_utf8_lossy(&out.stdout);
            let msg = msg.trim();
            if msg.is_empty() {
                println!("Removed RTK Claude hook (rtk init --global --uninstall).");
            } else {
                println!("{msg}");
            }
        }
        Ok(out) => {
            eprintln!(
                "warning: `rtk init --global --uninstall` exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Err(e) => {
            // No binary to run (or spawn failed) — nothing to unregister.
            eprintln!("rtk not installed or not runnable; nothing to uninstall ({e:#}).");
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
