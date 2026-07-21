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

use std::io::{IsTerminal, Write as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::rtk;
use crate::server::{READ_ONLY_TOOLS, WRITE_TOOLS};
use crate::session;

use crate::client;

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
    dry_run: bool,
}

/// CLI entry: `args` is everything after `setup`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let opts = parse_opts(args)?;

    // Auto-detect opencode (without --client or LENS_HOST) only if opencode on PATH
    // and claude is absent. Matches T2 spec. When both present, default remains
    // Claude (user must opt-in with --client opencode).
    if std::env::var("LENS_HOST").unwrap_or_default().trim().is_empty()
        && !std::env::args().any(|a| a == "--client")
        && cmd_exists("opencode")
        && !cmd_exists("claude")
    {
        std::env::set_var("LENS_HOST", "opencode");
    }

    // Make every downstream installer agree on which Claude config dir to write:
    // `claude_settings_path()`, `claude mcp add`, and the hook installers all read
    // `$CLAUDE_CONFIG_DIR`, so set it once up front when targeting a specific account.
    if let Some(dir) = &opts.config_dir {
        // use client abstraction so we set host-correct env var (T1)
        let var = if client::is_claude() { "CLAUDE_CONFIG_DIR" } else { "OPENCODE_CONFIG_DIR" };
        std::env::set_var(var, dir);
    }

    if client::is_claude() && !cmd_exists("claude") {
        bail!("Claude Code ('claude') not found on PATH. Install it first: https://claude.com/claude-code");
    }
    // opencode path does not require the `opencode` binary (we edit the jsonc directly)

    // 1. Copy this binary to a stable location; use THAT path everywhere so the
    //    MCP server + hooks keep working after the downloaded copy is deleted.
    //    In --dry-run we compute the target path without copying.
    let bin = if opts.dry_run {
        let name = if cfg!(windows) { "lens.exe" } else { "lens" };
        opts.bin_dir.join(name)
    } else {
        install_self(&opts.bin_dir).context("installing the lens binary")?
    };
    if opts.dry_run {
        say(&format!("dry-run: would install binary to {}", bin.display()));
    } else {
        say(&format!("Installed binary: {}", bin.display()));
    }

    // 2. Register the MCP server (the lens_* tools). Claude uses `claude mcp add`;
    //    opencode edits opencode.json(c) directly (no `claude` CLI needed).
    if opts.dry_run {
        if client::is_claude() {
            println!("dry-run: would run: claude mcp add lens --scope user -- {}", bin.display());
        } else {
            let cfg = opencode_config_file().unwrap_or_else(|_| PathBuf::from("~/.config/opencode/opencode.json"));
            let entry = build_opencode_mcp_entry(&bin, &opts.routing);
            println!("dry-run: would write MCP entry 'lens' to {}", cfg.display());
            if let Ok(pretty) = serde_json::to_string_pretty(&entry) {
                println!("{}", pretty);
            }
        }
    } else {
        let newly = if client::is_claude() {
            register_mcp_claude(&bin)
        } else {
            register_mcp_opencode(&bin, &opts.routing)
        };
        match newly {
            Ok(true) => say("Registered MCP server 'lens'."),
            Ok(false) => say("MCP server 'lens' already registered."),
            Err(e) => {
                let hint = if client::is_claude() {
                    format!("claude mcp add lens --scope user -- {}", bin.display())
                } else {
                    format!("edit {} to add under mcp.lens", opencode_config_file().map(|p| p.display().to_string()).unwrap_or_default())
                };
                warn(&format!("could not register MCP server: {e:#}\n  register by hand: {hint}"));
            }
        }
    }

    // 3-5b. Session hooks, RTK, routing and allow-list are Claude-specific (mcp__ prefixes,
    //    settings.json). For opencode: routing lives in the mcp "environment" we wrote;
    //    commands + the plugins/lens.js lifecycle bridge install below.
    if !opts.dry_run && client::detect_host() == client::Host::Claude {
        let settings = rtk::claude_settings_path()
            .ok_or_else(|| anyhow!("cannot resolve Claude settings path (is $HOME set?)"))?;

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

        // 5b. Pre-approve the lens tools so an agent is never blocked on a permission
        //     prompt for a lens call (plan mode prompts for any MCP tool not allow-listed).
        allow_lens_tools(&settings).context("allow-listing lens tools")?;
        say("Allow-listed lens tools.");
    } else if opts.dry_run {
        if client::is_claude() {
            println!("dry-run: would install session hooks + RTK + routing + allow-list into Claude settings.json");
        } else {
            println!("dry-run: MCP environment already carries LENS_ROUTING; would install commands (/dashboard, /warmup) + plugins/lens.js");
        }
    } else if !client::is_claude() {
        // Auto-install commands + the lifecycle plugin during setup for opencode so
        // `lens setup --client opencode` is complete immediately (doctor checks pass,
        // /dashboard usable, hooks bridge live on next opencode start). MCP was
        // registered above, so the plugin picks up the routing level from the entry.
        if let Some(cfg_dir) = client::config_dir_for(client::Host::Opencode) {
            let bin_str = bin.to_string_lossy().to_string();
            match session::install::install_opencode_assets(&cfg_dir, &bin_str) {
                Ok(()) => say("Installed commands (/dashboard, /warmup) + lifecycle plugin (plugins/lens.js)."),
                Err(e) => warn(&format!("could not install commands/plugin: {e:#}")),
            }
        } else {
            warn("could not resolve opencode config dir; commands/plugin not installed");
        }
    }

    // 6. PATH — so `lens` works as a bare command in new shells.
    let path_added = if opts.dry_run {
        false
    } else {
        ensure_on_path(&opts.bin_dir)
    };
    if opts.dry_run {
        say(&format!("dry-run: would ensure {} is on PATH", opts.bin_dir.display()));
    }

    // 7. Verify and report.
    println!();
    let ok = if opts.dry_run {
        println!("dry-run: would verify install (skipped side effects)");
        true
    } else if client::is_claude() {
        let settings = rtk::claude_settings_path()
            .ok_or_else(|| anyhow!("cannot resolve Claude settings path (is $HOME set?)"))?;
        doctor(&settings, &bin, &opts.bin_dir, path_added)
    } else {
        doctor_for_opencode(&bin, &opts.bin_dir, path_added)
    };
    println!();
    if ok {
        if client::is_claude() {
            println!("Done. Restart Claude Code to load lens (verify with the lens_stats tool).");
        } else {
            println!("Done. Restart opencode to load lens (verify with the lens_stats tool).");
        }
    } else {
        if client::is_claude() {
            println!("Setup finished with the warnings above. Restart Claude Code, then fix the flagged items or re-run `lens setup`.");
        } else {
            println!("Setup finished with the warnings above. Restart opencode, then fix the flagged items or re-run `lens setup --client opencode`.");
        }
    }
    if client::is_claude() {
        println!(
            "Uninstall: lens session uninstall && lens rtk uninstall && claude mcp remove lens && rm {}",
            bin.display()
        );
        // Offer the same lens-tool sync for the user's own custom subagents.
        offer_agent_sync();
    } else {
        println!(
            "Uninstall: lens session uninstall --client opencode && rm {}",
            bin.display()
        );
    }
    Ok(())
}

fn parse_opts(args: &[String]) -> Result<Opts> {
    let mut routing: Option<String> = None;
    let mut full = false;
    let mut bin_dir: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;
    let mut dry_run = false;

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
            "--client" => {
                // consumed early by client::from_cli_arg for Host detection; just skip value
                let _ = args.get(i + 1);
                i += 1;
            }
            "--dry-run" => dry_run = true,
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
        dry_run,
    })
}

/// Resolve the routing level: explicit `--routing` wins, else `full` (the default;
/// `--full` is kept for back-compat and is a no-op now that full is the default).
/// Rejects an unknown level.
fn resolve_routing(explicit: Option<&str>, full: bool) -> Result<String> {
    let _ = full;
    let level = explicit
        .map(|s| s.to_string())
        .unwrap_or_else(|| "full".into());
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
///
/// No `--cwd`/`--env` here on purpose: the server self-resolves its repo root at spawn
/// time (`server::resolve_repo_root`: `$LENS_DIR` > `$CLAUDE_PROJECT_DIR` > nearest
/// `.git` ancestor > cwd). `$CLAUDE_PROJECT_DIR` is how Claude Code's own env already
/// flows through to the spawned server at runtime, so no explicit registration is
/// needed for that branch either.
fn register_mcp_claude(bin: &Path) -> Result<bool> {
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

/// Returns whether 'lens' MCP is registered for the current host.
/// For Claude: via `claude mcp list`.
/// For opencode: via presence of an enabled entry in the opencode config.
fn mcp_registered() -> bool {
    if client::is_claude() {
        Command::new("claude")
            .args(["mcp", "list"])
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim_start().starts_with("lens"))
            })
            .unwrap_or(false)
    } else {
        match opencode_config_file() {
            Ok(p) => read_opencode_config(&p)
                .ok()
                .and_then(|v| {
                    // `enabled` is optional and defaults to true in opencode
                    // (same reading register_mcp_opencode uses).
                    v.get("mcp").and_then(|m| m.get("lens")).map(|l| {
                        l.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true)
                    })
                })
                .unwrap_or(false),
            Err(_) => false,
        }
    }
}

/// Resolve the opencode config file to edit (honors $OPENCODE_CONFIG_DIR).
/// opencode reads both `opencode.json` (the documented default) and
/// `opencode.jsonc`; edit whichever already exists so the registration can't
/// land in a file opencode ignores, defaulting to `opencode.json`.
fn opencode_config_file() -> Result<PathBuf> {
    let dir = client::config_dir_for(client::Host::Opencode)
        .context("cannot resolve opencode config dir (HOME or XDG_CONFIG_HOME not set?)")?;
    let json = dir.join("opencode.json");
    let jsonc = dir.join("opencode.jsonc");
    if !json.is_file() && jsonc.is_file() {
        return Ok(jsonc);
    }
    Ok(json)
}

/// Build the JSON value for the mcp.lens entry (used for dry-run print and write).
fn build_opencode_mcp_entry(bin: &Path, routing: &str) -> Value {
    let abs = std::fs::canonicalize(bin)
        .unwrap_or_else(|_| bin.to_path_buf())
        .to_string_lossy()
        .to_string();
    json!({
        "type": "local",
        "command": [abs],
        "enabled": true,
        "environment": {
            // Without LENS_HOST the spawned server process would detect_host()
            // as Claude and skip every opencode-specific branch.
            "LENS_HOST": "opencode",
            "LENS_ROUTING": routing
        }
    })
}

/// Read the opencode config, stripping simple // line comments (jsonc) for parse.
/// Block comments and complex cases not supported (note: write drops comments).
fn read_opencode_config(path: &Path) -> Result<Value> {
    if !path.is_file() {
        return Ok(json!({}));
    }
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let cleaned = strip_jsonc_line_comments(&raw);
    serde_json::from_str(&cleaned)
        .with_context(|| format!("parsing {} (jsonc)", path.display()))
}

fn strip_jsonc_line_comments(raw: &str) -> String {
    // minimal stateful strip of // comments, respecting "strings" (no \" handling for simplicity, but sufficient for config files)
    let mut out = String::with_capacity(raw.len());
    let mut in_string = false;
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' {
            in_string = !in_string;
            out.push(c);
            continue;
        }
        if !in_string && c == '/' && chars.peek() == Some(&'/') {
            chars.next(); // consume second /
            // skip to eol or end
            for cc in chars.by_ref() {
                if cc == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Edit the opencode config to register lens under mcp (idempotent on matching bin).
/// Creates file/dir if needed. Returns Ok(true) if changed, Ok(false) if already
/// present+enabled with the requested routing. A matching entry with a different
/// routing gets only its environment.LENS_ROUTING updated (user tweaks preserved).
fn register_mcp_opencode(bin: &Path, routing: &str) -> Result<bool> {
    let cfg_path = opencode_config_file()?;
    let mut root = read_opencode_config(&cfg_path)?;
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    // preserve/ensure schema like real opencode configs
    obj.entry("$schema")
        .or_insert(json!("https://opencode.ai/config.json"));

    let mcp = obj.entry("mcp").or_insert_with(|| json!({}));
    if !mcp.is_object() {
        *mcp = json!({});
    }
    let mcp_obj = mcp.as_object_mut().unwrap();

    let abs = std::fs::canonicalize(bin)
        .unwrap_or_else(|_| bin.to_path_buf())
        .to_string_lossy()
        .to_string();

    let command_matches = mcp_obj.get("lens").is_some_and(|cur| {
        cur.get("command") == Some(&json!([abs]))
            && cur.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true)
    });

    if command_matches {
        let cur = mcp_obj.get_mut("lens").unwrap();
        let env_get = |key: &str| {
            cur.get("environment")
                .and_then(|e| e.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        if env_get("LENS_HOST").as_deref() == Some("opencode")
            && env_get("LENS_ROUTING").as_deref() == Some(routing)
        {
            return Ok(false);
        }
        // Same binary, stale environment: patch only the lens-owned keys.
        let env = cur
            .as_object_mut()
            .unwrap()
            .entry("environment")
            .or_insert_with(|| json!({}));
        if !env.is_object() {
            *env = json!({});
        }
        let env = env.as_object_mut().unwrap();
        env.insert("LENS_HOST".to_string(), json!("opencode"));
        env.insert("LENS_ROUTING".to_string(), json!(routing));
    } else {
        mcp_obj.insert("lens".to_string(), build_opencode_mcp_entry(bin, routing));
    }
    write_json(&cfg_path, &root)?;
    Ok(true)
}

/// Remove the `mcp.lens` entry from opencode's config (the inverse of
/// `register_mcp_opencode`). Returns Ok(true) if an entry was removed.
/// Best-effort counterpart to `claude mcp remove lens` on the Claude path.
pub fn unregister_mcp_opencode() -> Result<bool> {
    let cfg_path = opencode_config_file()?;
    if !cfg_path.is_file() {
        return Ok(false);
    }
    let mut root = read_opencode_config(&cfg_path)?;
    let removed = root
        .get_mut("mcp")
        .and_then(Value::as_object_mut)
        .map(|m| m.remove("lens").is_some())
        .unwrap_or(false);
    if removed {
        write_json(&cfg_path, &root)?;
    }
    Ok(removed)
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

/// Merge `permissions.allow` entries `mcp__lens__<tool>` for every lens tool into
/// `settings`, preserving existing entries (idempotent, no duplicates). Claude Code
/// then auto-runs a lens call instead of prompting, so an unattended agent never
/// stalls on a permission dialog (Bug B). Plan mode's MCP auto-deny runs before the
/// allow-list check and ignores per-tool entries (anthropics/claude-code#12368);
/// the confirmed workaround is a scoped wildcard, so `mcp__lens__*` is added too.
/// The tool lists are [`READ_ONLY_TOOLS`] + [`WRITE_TOOLS`], shared with the server
/// so they never drift.
/// (opencode path skips this entirely; bare lens_* tools, different permission model)
fn allow_lens_tools(settings: &Path) -> Result<()> {
    let mut root = read_json(settings)?;
    if !root.is_object() {
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();
    let perms = obj.entry("permissions").or_insert_with(|| json!({}));
    if !perms.is_object() {
        *perms = json!({});
    }
    let allow = perms
        .as_object_mut()
        .unwrap()
        .entry("allow")
        .or_insert_with(|| json!([]));
    if !allow.is_array() {
        *allow = json!([]);
    }
    let arr = allow.as_array_mut().unwrap();
    for tool in READ_ONLY_TOOLS.iter().chain(WRITE_TOOLS.iter()) {
        let entry = Value::from(format!("mcp__lens__{tool}"));
        if !arr.contains(&entry) {
            arr.push(entry);
        }
    }
    let wildcard = Value::from("mcp__lens__*");
    if !arr.contains(&wildcard) {
        arr.push(wildcard);
    }
    // The bundled slash commands (/dashboard, /warmup) and the update nudge have the
    // model run the lens CLI via Bash, and /dashboard probes its local port with curl;
    // pre-approve those too so no lens flow ever stalls on a permission prompt.
    for rule in ["Bash(lens:*)", "Bash(curl -s http://127.0.0.1:*)"] {
        let entry = Value::from(rule);
        if !arr.contains(&entry) {
            arr.push(entry);
        }
    }
    write_json(settings, &root)
}

// ── custom-agent lens tool sync ─────────────────────────────────────────────
//
// A subagent whose frontmatter omits `tools:` (or sets it to a bare `*`) reads as
// "all tools," but Claude Code's wildcard grant does not reliably wire MCP tools
// into a subagent running inside an isolated git worktree, while naming
// `mcp__lens__<tool>` explicitly in `tools:` does. Converting an implicit
// "everything" grant into an explicit list is a real narrowing (any tool added
// later needs a manual re-edit), so this only ever appends to an agent that
// ALREADY has an explicit `tools:` list; it never invents one.

/// An agent file whose explicit `tools:` frontmatter line is missing lens tools.
/// `line_start`/`line_end` bound that line (no trailing newline) in the file's text,
/// re-validated at patch time in case the file changed between scan and patch.
struct AgentGap {
    path: PathBuf,
    name: String,
    line_start: usize,
    line_end: usize,
    missing: Vec<String>,
}

/// `$CLAUDE_CONFIG_DIR/agents` (may be a symlinked directory, e.g. to
/// `~/.claude-personal/agents`; reading/writing files inside it follows the
/// symlink like any other directory, so no special-casing is needed).
fn agents_dir() -> Option<PathBuf> {
    rtk::claude_config_dir().map(|d| d.join("agents"))
}

/// Every `*.md` file directly under `dir` with an explicit, non-wildcard `tools:`
/// frontmatter line missing at least one lens tool. Agents with no `tools:` line,
/// or `tools: "*"`, are skipped — see the module-level note above.
fn scan_agent_gaps(dir: &Path) -> Result<Vec<AgentGap>> {
    let all_tools: Vec<String> = READ_ONLY_TOOLS
        .iter()
        .chain(WRITE_TOOLS.iter())
        .map(|t| format!("mcp__lens__{t}"))
        .collect();

    let mut gaps = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if !text.starts_with("---\n") {
            continue; // no frontmatter: not an agent definition
        }

        let mut offset = 4; // past the opening "---\n"
        let mut name = None;
        let mut tools_span = None;
        for line in text[4..].split_inclusive('\n') {
            let trimmed = line.trim_end_matches('\n');
            if trimmed == "---" {
                break; // end of frontmatter
            }
            if let Some(rest) = trimmed.strip_prefix("name:") {
                name = Some(rest.trim().to_string());
            } else if let Some(rest) = trimmed.strip_prefix("tools:") {
                tools_span = Some((offset, offset + trimmed.len(), rest.trim().to_string()));
            }
            offset += line.len();
        }

        let Some((line_start, line_end, value)) = tools_span else {
            continue; // no explicit tools: line: implicit "all tools", leave it alone
        };
        if matches!(value.as_str(), "*" | "\"*\"" | "'*'") {
            continue; // explicit wildcard: same as no list, leave it alone
        }
        let present: Vec<&str> = value.split(',').map(str::trim).collect();
        let missing: Vec<String> = all_tools
            .iter()
            .filter(|t| !present.contains(&t.as_str()))
            .cloned()
            .collect();
        if missing.is_empty() {
            continue;
        }
        gaps.push(AgentGap {
            name: name.unwrap_or_else(|| path.file_stem().unwrap().to_string_lossy().to_string()),
            path,
            line_start,
            line_end,
            missing,
        });
    }
    gaps.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(gaps)
}

/// Append `gap`'s missing lens tools to its `tools:` line in place.
fn patch_agent_gap(gap: &AgentGap) -> Result<()> {
    let text = std::fs::read_to_string(&gap.path)
        .with_context(|| format!("reading {}", gap.path.display()))?;
    let still_valid = text
        .get(gap.line_start..gap.line_end)
        .is_some_and(|s| s.trim_start().starts_with("tools:"));
    if !still_valid {
        bail!(
            "{}: tools: line moved since scanning, skipping (re-run to retry)",
            gap.path.display()
        );
    }
    let mut patched = String::with_capacity(text.len() + 16 * gap.missing.len());
    patched.push_str(&text[..gap.line_end]);
    patched.push_str(", ");
    patched.push_str(&gap.missing.join(", "));
    patched.push_str(&text[gap.line_end..]);
    std::fs::write(&gap.path, patched).with_context(|| format!("writing {}", gap.path.display()))
}

/// Interactively offer to patch each gap under `agents_dir()`, one agent at a time —
/// some agents (e.g. a deliberately lens-free A/B control arm) may be missing lens
/// tools on purpose, so this asks per agent rather than one blanket yes/no that could
/// patch an agent the user never meant to change. No-op if the directory doesn't
/// exist, nothing is missing, or stdin isn't a terminal (an unattended install must
/// never block waiting for input).
fn offer_agent_sync() {
    let Some(dir) = agents_dir() else { return };
    if !dir.is_dir() {
        return;
    }
    let gaps = match scan_agent_gaps(&dir) {
        Ok(g) => g,
        Err(e) => {
            warn(&format!("could not scan agents in {}: {e:#}", dir.display()));
            return;
        }
    };
    if gaps.is_empty() || !std::io::stdin().is_terminal() {
        return;
    }

    println!();
    println!(
        "Found {} custom agent(s) in {} without full lens tool access.",
        gaps.len(),
        dir.display()
    );
    for gap in &gaps {
        print!(
            "  - {} is missing {} lens tool{}. Add them? [y/N] ",
            gap.name,
            gap.missing.len(),
            if gap.missing.len() == 1 { "" } else { "s" }
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return;
        }
        if !answer.trim().eq_ignore_ascii_case("y") {
            continue;
        }
        match patch_agent_gap(gap) {
            Ok(()) => say(&format!("Updated {}", gap.path.display())),
            Err(e) => warn(&format!("{e:#}")),
        }
    }
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
/// Host-aware via mcp_registered(); claude-only checks inside.
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

/// Opencode variant of doctor: only checks things that apply without claude settings.json.
/// (MCP via the opencode config, commands, the lifecycle plugin, and PATH.)
fn doctor_for_opencode(bin: &Path, bin_dir: &Path, path_added: bool) -> bool {
    let mut checks: Vec<(String, bool, String)> = Vec::new();

    checks.push(("MCP server registered".into(), mcp_registered(), String::new()));

    // commands + plugin live under the opencode config dir (not settings.json parent)
    let op_dir = client::config_dir_for(client::Host::Opencode).unwrap_or_default();
    let cmd_present = op_dir.join("commands").join("dashboard.md").is_file();
    checks.push(("/dashboard command installed".into(), cmd_present, String::new()));

    let plugin_present = op_dir.join("plugins").join("lens.js").is_file();
    checks.push(("lifecycle plugin installed".into(), plugin_present, String::new()));

    // no RTK equivalent yet for opencode
    checks.push(("RTK shell compression".into(), true, "Claude-specific today; not required for opencode".into()));

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

    println!("Verifying (opencode):");
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
/// Hits the public GitHub release over `curl` (no auth, no `gh`).
pub fn run_update_cli(args: &[String]) -> Result<()> {
    let config_dir = parse_config_dir(args);
    if let Some(dir) = &config_dir {
        // use client abstraction so we set host-correct env var (T1)
        let var = if client::is_claude() { "CLAUDE_CONFIG_DIR" } else { "OPENCODE_CONFIG_DIR" };
        std::env::set_var(var, dir);
    }

    if !cmd_exists("curl") {
        bail!("`lens update` needs `curl` to reach the public release. Install curl, then retry.");
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
    let routing = if client::is_claude() {
        let settings = rtk::claude_settings_path()
            .ok_or_else(|| anyhow!("cannot resolve Claude settings path"))?;
        current_routing(&settings).unwrap_or_else(|| "full".to_string())
    } else {
        // for opencode read from the mcp env if present, else default
        read_opencode_routing().unwrap_or_else(|| "full".to_string())
    };

    let mut cmd = Command::new(&tmp);
    cmd.arg("setup").arg("--routing").arg(&routing);
    if !client::is_claude() {
        cmd.arg("--client").arg("opencode");
    }
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
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        (os, arch) => bail!("no prebuilt lens binary for {os}/{arch}; build + `setup` from source"),
    })
}

/// Latest published release tag, read with no auth: `/releases/latest` 302-redirects
/// to `/releases/tag/<tag>`, so follow it with `curl` and take the tag from the final URL.
fn latest_tag(repo: &str) -> Result<String> {
    let url = format!("https://github.com/{repo}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSLI", "-o", "/dev/null", "-w", "%{url_effective}", &url])
        .output()
        .context("running `curl` to resolve the latest release tag")?;
    if !out.status.success() {
        bail!(
            "resolving the latest release of {repo} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let effective = String::from_utf8_lossy(&out.stdout);
    tag_from_release_url(&effective)
        .ok_or_else(|| anyhow!("no releases found for {repo} (resolved to {})", effective.trim()))
}

/// Extract the tag from a `…/releases/tag/<tag>` URL. `None` if the URL doesn't name a
/// tag (e.g. a repo with no releases redirects to `…/releases`).
fn tag_from_release_url(url: &str) -> Option<String> {
    let (_, tag) = url.trim().rsplit_once("/releases/tag/")?;
    let tag = tag.trim_matches('/');
    if tag.is_empty() || tag.contains('/') {
        return None;
    }
    Some(tag.to_string())
}

/// Download `lens-<target>` from `tag` to `dest` over the public release URL via `curl`.
fn download_release(repo: &str, tag: &str, target: &str, dest: &Path) -> Result<()> {
    let url = format!("https://github.com/{repo}/releases/download/{tag}/lens-{target}");
    let out = Command::new("curl")
        .args(["-fsSL", "-o"])
        .arg(dest)
        .arg(&url)
        .output()
        .context("running `curl` to download the release binary")?;
    if !out.status.success() {
        bail!(
            "downloading {url} failed: {}",
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

/// Read LENS_ROUTING from opencode mcp.lens.environment (for update re-invoke).
pub(crate) fn read_opencode_routing() -> Option<String> {
    let path = opencode_config_file().ok()?;
    read_opencode_config(&path)
        .ok()?
        .get("mcp")?
        .get("lens")?
        .get("environment")?
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

// ── update-available nudge ───────────────────────────────────────────────────
//
// The SessionStart hook calls `update_nudge_line()`, which only *reads* a cached
// result so it never blocks the session on the network. When the cache is stale it
// spawns a detached `lens __update-check` to refresh it for next time.

/// How long a cached update-check stays fresh before a background refresh.
const UPDATE_TTL_SECS: u64 = 24 * 60 * 60;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Global, per-user cache file for the latest-release check.
fn update_cache_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")))?;
    Some(base.join("lens").join("update-check.json"))
}

/// `(cached_tag, fresh)` from the cache file. Missing/unparseable reads as `(None, false)`
/// so a stale or absent cache triggers a background refresh.
fn read_update_cache() -> (Option<String>, bool) {
    let Some(path) = update_cache_path() else {
        return (None, false);
    };
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return (None, false);
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return (None, false);
    };
    let tag = v
        .get("latest_tag")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string());
    let checked_at = v.get("checked_at").and_then(|t| t.as_u64()).unwrap_or(0);
    let fresh = now_unix().saturating_sub(checked_at) < UPDATE_TTL_SECS;
    (tag, fresh)
}

/// Spawn a detached `lens __update-check` to refresh the cache; returns immediately.
fn spawn_update_check() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .arg("__update-check")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// One-line "update available" nudge for SessionStart, or `None` when up to date, opted
/// out (`LENS_NO_UPDATE_CHECK`), or the cache has nothing yet. Reads the cache only and
/// kicks off a detached refresh when stale, so it never waits on the network.
pub fn update_nudge_line() -> Option<String> {
    if std::env::var_os("LENS_NO_UPDATE_CHECK").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let (cached_tag, fresh) = read_update_cache();
    if !fresh {
        spawn_update_check();
    }
    let tag = cached_tag?;
    let current = env!("CARGO_PKG_VERSION");
    is_newer(&tag, current)
        .then(|| format!("lens {tag} is available (you're on v{current}). Update: lens update"))
}

/// `lens __update-check`: refresh the cached latest-release tag. Best-effort and silent;
/// always stamps `checked_at` (even on a failed fetch) so it backs off for the full TTL.
pub fn run_update_check_cli() {
    let tag = latest_tag(&repo()).ok();
    let Some(path) = update_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let body = json!({ "checked_at": now_unix(), "latest_tag": tag });
    let _ = std::fs::write(&path, body.to_string());
}

/// CLI entry for `lens doctor` (standalone verification, no setup side-effects).
/// Reuses the host-aware doctor logic. Accepts --client, --config-dir (to target
/// specific accounts without mutating env for caller), --bin-dir (for PATH check).
/// Reports per-host checks; `lens doctor --client opencode` works without claude bin.
pub fn run_doctor_cli(args: &[String]) -> Result<()> {
    // Parse enough to honor --config-dir (sets host-specific env like run_cli does)
    // and --bin-dir. --client is consumed by client::from_cli_arg via detect.
    let mut config_dir: Option<PathBuf> = None;
    let mut bin_dir: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config-dir" => {
                if let Some(d) = args.get(i + 1) {
                    config_dir = Some(PathBuf::from(d));
                    i += 1;
                }
            }
            "--bin-dir" => {
                if let Some(d) = args.get(i + 1) {
                    bin_dir = Some(PathBuf::from(d));
                    i += 1;
                }
            }
            "--client" => {
                let _ = args.get(i + 1);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    if let Some(dir) = &config_dir {
        let var = if client::is_claude() { "CLAUDE_CONFIG_DIR" } else { "OPENCODE_CONFIG_DIR" };
        std::env::set_var(var, dir);
    }

    let host = client::detect_host();
    let bin = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("lens"));
    let bin_dir = bin_dir.unwrap_or_else(|| bin.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from(".")));
    let path_added = false;

    let ok = if host == client::Host::Claude {
        let settings = rtk::claude_settings_path()
            .ok_or_else(|| anyhow!("cannot resolve Claude settings path (is $HOME set?)"))?;
        doctor(&settings, &bin, &bin_dir, path_added)
    } else {
        doctor_for_opencode(&bin, &bin_dir, path_added)
    };
    if !ok {
        // non-fatal for doctor CLI; caller sees FAILs in output
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn resolve_routing_defaults_and_validates() {
        assert_eq!(resolve_routing(None, false).unwrap(), "full"); // full is the default
        assert_eq!(resolve_routing(None, true).unwrap(), "full");
        // Explicit wins over the default.
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
    fn allow_lens_tools_merges_and_is_idempotent() {
        let dir = tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&json!({ "env": { "EXISTING": "1" } })).unwrap(),
        )
        .unwrap();

        allow_lens_tools(&settings).unwrap();
        let root = read_json(&settings).unwrap();
        let allow = root["permissions"]["allow"].as_array().unwrap();
        assert!(
            allow.iter().any(|v| v == "mcp__lens__lens_search"),
            "read-only search tool must be allow-listed"
        );
        assert!(allow.iter().any(|v| v == "mcp__lens__lens_overview"));
        for tool in WRITE_TOOLS {
            let entry = format!("mcp__lens__{tool}");
            assert!(
                allow.iter().any(|v| v == entry.as_str()),
                "write tool {tool} must be allow-listed (plan mode prompts otherwise)"
            );
        }
        assert!(
            allow.iter().any(|v| v == "Bash(lens:*)"),
            "lens CLI must be allow-listed for the bundled slash commands"
        );
        assert!(allow.iter().any(|v| v == "Bash(curl -s http://127.0.0.1:*)"));
        // Named regression checks for the memory tools: a rename or removal from
        // WRITE_TOOLS/READ_ONLY_TOOLS must fail loudly here, not just silently drop
        // out of the generic loop above.
        assert!(allow.iter().any(|v| v == "mcp__lens__lens_memory_record"));
        assert!(allow.iter().any(|v| v == "mcp__lens__lens_memory_query"));
        assert!(
            allow.iter().any(|v| v == "mcp__lens__*"),
            "wildcard must be allow-listed (plan mode ignores per-tool entries, anthropics/claude-code#12368)"
        );
        assert_eq!(root["env"]["EXISTING"], "1", "other keys preserved");

        // Re-running must not duplicate entries.
        allow_lens_tools(&settings).unwrap();
        let root2 = read_json(&settings).unwrap();
        let count = root2["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| *v == "mcp__lens__lens_search")
            .count();
        assert_eq!(count, 1, "re-run must not duplicate allow entries");
        let wildcard_count = root2["permissions"]["allow"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| *v == "mcp__lens__*")
            .count();
        assert_eq!(wildcard_count, 1, "re-run must not duplicate the wildcard entry");
    }

    #[test]
    fn scan_agent_gaps_flags_partial_lists_and_skips_wildcards() {
        let dir = tempdir().unwrap();
        std::fs::write(
            dir.path().join("partial.md"),
            "---\nname: partial\ntools: Read, Grep, mcp__lens__lens_search\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("complete.md"),
            format!(
                "---\nname: complete\ntools: Read, {}\n---\nbody\n",
                READ_ONLY_TOOLS
                    .iter()
                    .chain(WRITE_TOOLS.iter())
                    .map(|t| format!("mcp__lens__{t}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("implicit.md"),
            "---\nname: implicit\ncolor: red\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wildcard.md"),
            "---\nname: wildcard\ntools: \"*\"\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("README.md"), "not an agent\n").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "tools: nope\n").unwrap();

        let gaps = scan_agent_gaps(dir.path()).unwrap();
        let names: Vec<&str> = gaps.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["partial"],
            "no tools: line, a wildcard, and an already-complete list are all left alone"
        );
        assert!(gaps[0].missing.contains(&"mcp__lens__lens_symbol".to_string()));
        assert!(!gaps[0].missing.contains(&"mcp__lens__lens_search".to_string()));
    }

    #[test]
    fn patch_agent_gap_appends_missing_tools_and_preserves_rest() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("partial.md");
        std::fs::write(
            &path,
            "---\nname: partial\ntools: Read, Grep\ncolor: blue\n---\n\n# body\ntext\n",
        )
        .unwrap();

        let gaps = scan_agent_gaps(dir.path()).unwrap();
        assert_eq!(gaps.len(), 1);
        patch_agent_gap(&gaps[0]).unwrap();

        let patched = std::fs::read_to_string(&path).unwrap();
        assert!(patched.contains("tools: Read, Grep, mcp__lens__lens_search"));
        assert!(
            patched.contains("color: blue"),
            "later frontmatter keys preserved"
        );
        assert!(patched.contains("# body\ntext"), "body preserved");

        // Re-scanning the patched file finds no remaining gap.
        assert!(scan_agent_gaps(dir.path()).unwrap().is_empty());
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

    #[test]
    fn tag_parsed_from_release_redirect_url() {
        assert_eq!(
            tag_from_release_url("https://github.com/DemoDevelops/lens/releases/tag/v0.3.0"),
            Some("v0.3.0".to_string())
        );
        // Trailing slash / whitespace tolerated.
        assert_eq!(
            tag_from_release_url(" https://github.com/o/r/releases/tag/v1.2.3/ \n"),
            Some("v1.2.3".to_string())
        );
        // A repo with no releases redirects to the releases index, not a tag.
        assert_eq!(
            tag_from_release_url("https://github.com/o/r/releases"),
            None
        );
    }

    #[test]
    fn register_mcp_opencode_creates_entry_and_is_idempotent() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let cfg_dir = dir.path().join("opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg_dir.to_str().unwrap());
        let bin = PathBuf::from("/tmp/fake-lens-register-test");
        let newly = register_mcp_opencode(&bin, "full").unwrap();
        assert!(newly, "first registration must report newly added");
        let cfg_path = opencode_config_file().unwrap();
        let root = read_opencode_config(&cfg_path).unwrap();
        assert_eq!(root["$schema"], "https://opencode.ai/config.json");
        let lens = &root["mcp"]["lens"];
        assert_eq!(lens["type"], "local");
        assert_eq!(lens["command"], json!(["/tmp/fake-lens-register-test"]));
        assert_eq!(lens["enabled"], true);
        assert_eq!(lens["environment"]["LENS_HOST"], "opencode");
        assert_eq!(lens["environment"]["LENS_ROUTING"], "full");
        let again = register_mcp_opencode(&bin, "full").unwrap();
        assert!(!again, "re-registration must be no-op (returns false)");
        // a different routing level updates the entry in place
        let rerouted = register_mcp_opencode(&bin, "nudge").unwrap();
        assert!(rerouted, "routing change must rewrite the entry");
        let root = read_opencode_config(&cfg_path).unwrap();
        assert_eq!(root["mcp"]["lens"]["environment"]["LENS_ROUTING"], "nudge");
        assert_eq!(root["mcp"]["lens"]["environment"]["LENS_HOST"], "opencode");
        std::env::remove_var("OPENCODE_CONFIG_DIR");
    }

    #[test]
    fn register_mcp_opencode_handles_existing_jsonc_and_preserves_other_keys() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let cfg_dir = dir.path().join("opencode2");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("opencode.jsonc");
        // pre-existing with line comment (jsonc) and other mcp entry
        let initial = r#"{
  "$schema": "https://opencode.ai/config.json",
  // existing comment
  "mcp": {
    "other": {"type": "stdio", "command": ["foo"]}
  }
}"#;
        std::fs::write(&cfg_path, initial).unwrap();
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg_dir.to_str().unwrap());
        let bin = PathBuf::from("/tmp/fake-lens2");
        let newly = register_mcp_opencode(&bin, "full").unwrap();
        assert!(newly);
        let root = read_opencode_config(&cfg_path).unwrap();
        assert!(root["mcp"]["other"].is_object());
        let lens = &root["mcp"]["lens"];
        assert_eq!(lens["command"], json!(["/tmp/fake-lens2"]));
        // comments are dropped on write (per fn doc), but other keys preserved
        let again = register_mcp_opencode(&bin, "full").unwrap();
        assert!(!again);
        std::env::remove_var("OPENCODE_CONFIG_DIR");
    }

    #[test]
    fn parse_opts_accepts_client_flag_without_error() {
        let args: Vec<String> = ["--client", "opencode", "--routing", "nudge"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let opts = parse_opts(&args).unwrap();
        assert_eq!(opts.routing, "nudge");
        // --client consumed by from_cli_arg for host; parse_opts just skips the flag
    }

    #[test]
    fn mcp_registered_detects_opencode_mcp_entry() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let cfg_dir = dir.path().join("oc-reg");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        let cfg_path = cfg_dir.join("opencode.jsonc");
        std::fs::write(
            &cfg_path,
            r#"{"mcp": {"lens": {"enabled": true, "command": ["/tmp/l"] } } }"#,
        )
        .unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg_dir.to_str().unwrap());
        assert!(mcp_registered());
        std::env::remove_var("LENS_HOST");
        std::env::remove_var("OPENCODE_CONFIG_DIR");
    }

    #[test]
    fn doctor_for_opencode_checks_mcp_and_commands() {
        let _g = crate::rtk::env_test_lock();
        let dir = tempdir().unwrap();
        let cfg_dir = dir.path().join("oc-doc");
        std::fs::create_dir_all(cfg_dir.join("commands")).unwrap();
        let cfg_path = cfg_dir.join("opencode.jsonc");
        std::fs::write(
            &cfg_path,
            r#"{"$schema":"https://opencode.ai/config.json","mcp":{"lens":{"type":"local","command":["/bin/fake-lens"],"enabled":true}}}"#,
        )
        .unwrap();
        std::fs::write(
            cfg_dir.join("commands").join("dashboard.md"),
            "# dashboard",
        )
        .unwrap();
        std::fs::create_dir_all(cfg_dir.join("plugins")).unwrap();
        std::fs::write(cfg_dir.join("plugins").join("lens.js"), "// lens plugin").unwrap();
        std::env::set_var("LENS_HOST", "opencode");
        std::env::set_var("OPENCODE_CONFIG_DIR", cfg_dir.to_str().unwrap());
        let bin = std::env::current_exe().unwrap();
        let bin_dir = bin.parent().unwrap().to_path_buf();
        let oldp = std::env::var_os("PATH");
        let p = format!(
            "{}:{}",
            bin_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        std::env::set_var("PATH", &p);
        let ok = doctor_for_opencode(&bin, &bin_dir, false);
        if let Some(op) = oldp {
            std::env::set_var("PATH", op);
        } else {
            std::env::remove_var("PATH");
        }
        std::env::remove_var("LENS_HOST");
        std::env::remove_var("OPENCODE_CONFIG_DIR");
        assert!(ok);
    }
}
