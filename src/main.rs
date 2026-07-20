//! lens entrypoint.
//!
//! Two modes:
//!   * No subcommand → run the MCP stdio server (the default).
//!   * `lens hook <platform> <event>` → a short-lived session-continuity
//!     lifecycle hook (stdin = hook payload, stdout = hook response).
//!   * `lens session <install|uninstall|status>` → manage the hooks.
//!   * `lens setup [--full]` → self-install for the current user: copy onto PATH,
//!     register the MCP server, install hooks (clearing Context Mode) + RTK, set the
//!     routing level, then verify.
//!   * `lens update` → if a newer release exists, download the matching binary and
//!     re-run `setup` with it (preserving routing level + install location).
//!   * `lens warmup [path]` → build the code graph + FTS index for a repo up
//!     front, so lens_symbol / lens_search work without the server's lazy first build.
//!   * `lens stats [...]` / `lens verify [...]` → read-only observability
//!     views over the op log + reversible store (separate processes, own stdout).
//!
//! CRITICAL: in server mode stdout is the JSON-RPC channel. NOTHING may be
//! written to stdout except the MCP transport. All logging/diagnostics go to
//! stderr (and the op/explain logs go to files). The hook/session/stats/verify
//! subcommands are separate processes whose stdout is their own response channel;
//! they keep logging on stderr too.

use anyhow::Result;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use lens::obs;
use lens::server::Forge;
use lens::session;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("hook") => return session::hook::run_cli(&args[2..]),
        Some("session") => return session::install::run_cli(&args[2..]),
        Some("setup") => return lens::setup::run_cli(&args[2..]),
        Some("update") => return lens::setup::run_update_cli(&args[2..]),
        // Hidden: refresh the cached latest-release tag for the SessionStart nudge.
        // Spawned detached by the hook; always silent, never errors out.
        Some("__update-check") => {
            lens::setup::run_update_check_cli();
            return Ok(());
        }
        Some("stats") => return obs::stats::run_cli(&args[2..]),
        Some("verify") => return obs::verify::run_cli(&args[2..]),
        Some("dashboard") => return obs::dashboard::run_cli(&args[2..]),
        Some("top") => {
            // Ergonomic alias: `lens top` == `lens dashboard --tui`.
            let mut a = vec!["--tui".to_string()];
            a.extend_from_slice(&args[2..]);
            return obs::dashboard::run_cli(&a);
        }
        Some("wrap") => return lens::wrap::run_cli(&args[2..]),
        Some("rtk") => return lens::rtk::run_cli(&args[2..]),
        Some("warmup") => return lens::warmup::run_cli(&args[2..]),
        Some("q") => return lens::qcli::run_cli(&args[2..]),
        Some("watch") => return lens::warmup::run_watch_cli(&args[2..]),
        Some("--version") | Some("-V") => {
            println!("lens {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            print_usage();
            return Ok(());
        }
        _ => {}
    }
    // `--explain` is an alias for LENS_EXPLAIN=1 (opt-in per-op trace).
    if args.iter().any(|a| a == "--explain") {
        std::env::set_var("LENS_EXPLAIN", "1");
    }
    run_server()
}

fn print_usage() {
    println!("lens — code graph + FTS index over a repo, exposed as an MCP server");
    println!();
    println!("USAGE:");
    println!("    lens                        run the MCP stdio server (default)");
    println!("    lens hook <platform> <event>");
    println!("    lens session <install|uninstall|status>");
    println!("    lens setup [--full]");
    println!("    lens update");
    println!("    lens warmup [path]");
    println!("    lens watch [path]");
    println!("    lens q <verb> [args]        read-only queries over an existing .lens/");
    println!("    lens dashboard [--port <n>] [--tui ...]");
    println!("    lens top                    alias for `dashboard --tui`");
    println!("    lens stats [...]");
    println!("    lens verify [...]");
    println!("    lens wrap ...");
    println!("    lens rtk ...");
    println!("    lens --version, -V          print the version");
    println!("    lens --help, -h             print this message");
}

#[tokio::main]
async fn run_server() -> Result<()> {
    // Logging to stderr only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let forge = Forge::new()?;
    tracing::info!("lens starting on stdio");

    // Liveness heartbeat for the routing layer's MCP-ready guard (a separate
    // hook process): it treats the server as reachable while ANY file under
    // `<data_dir>/heartbeats/` is fresh. Each server process owns exactly one
    // file, named after its own pid, so two servers sharing a data dir (two
    // sessions in the same repo) never touch each other's liveness signal —
    // there's no shared mutable state to race on, by construction. Re-touched
    // periodically so a crashed server's file goes stale and routing falls
    // back to passthrough.
    let pidfile = heartbeat_path();
    if let Some(p) = &pidfile {
        if let Some(dir) = p.parent() {
            prune_stale_heartbeats(dir, p);
        }
        write_heartbeat(p);
        let p = p.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                write_heartbeat(&p);
            }
        });
    }

    // Capture the data dir before `serve` consumes the forge, so we can tidy the
    // SQLite WAL sidecars on a clean shutdown below.
    let data_dir = forge.data_dir().to_path_buf();

    let service = forge.serve(stdio()).await?;
    service.waiting().await?;
    if let Some(p) = &pidfile {
        // Safe unconditionally: the filename is our own pid, so no other lens
        // process can be relying on this exact path for its own liveness.
        let _ = std::fs::remove_file(p);
    }
    finalize_wal_files(&data_dir);
    Ok(())
}

/// On a clean shutdown (the MCP client disconnected), checkpoint each WAL database
/// fully into its main file and switch it out of WAL mode, then unlink the now-stale
/// `-wal`/`-shm` sidecars so the data dir is tidy. The next start re-enables WAL via
/// `configure_conn`. macOS leaves the `-shm` behind even after the mode switch, so the
/// removal is explicit; it is safe only because the checkpoint already folded all WAL
/// content into the main db. Best-effort: skipped on SIGKILL (no clean exit runs).
fn finalize_wal_files(data_dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(data_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        // Scope the connection so it is closed before we unlink the sidecars.
        let checkpointed = match rusqlite::Connection::open(&path) {
            Ok(conn) => conn
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")
                .is_ok(),
            Err(_) => false,
        };
        if checkpointed {
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = path.clone().into_os_string();
                sidecar.push(suffix);
                let _ = std::fs::remove_file(sidecar);
            }
        }
    }
}

/// Resolve this process's own heartbeat file, `<data_dir>/heartbeats/<pid>.pid`
/// (data dir from `$LENS_DIR`, else `<cwd>/.lens`). `None` if unresolvable.
/// Naming the file after the pid — rather than a single shared `server.pid` —
/// means concurrent lens servers in the same data dir each own a distinct path:
/// no process ever writes or deletes another's liveness file.
fn heartbeat_path() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("LENS_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir().ok()?.join(".lens"),
    };
    let dir = base.join("heartbeats");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!("{}.pid", std::process::id())))
}

/// Best-effort write of the current pid; updates mtime so freshness checks pass.
fn write_heartbeat(path: &std::path::Path) {
    let _ = std::fs::write(path, std::process::id().to_string());
}

/// Best-effort cleanup of sibling heartbeat files left behind by servers that
/// never ran their own shutdown path (SIGKILL, OOM). Only removes files well
/// past any reasonable TTL (10 minutes — several multiples of the 30s beat and
/// the routing layer's default 90s TTL) so it can never race a peer that's
/// merely between two beats. Purely hygiene: `mcp_ready` already ignores stale
/// entries on its own, this just keeps the directory from growing unbounded on
/// a long-lived dev machine.
fn prune_stale_heartbeats(dir: &std::path::Path, own: &std::path::Path) {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == own {
            continue;
        }
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(|m| m.elapsed().map(|age| age > STALE_AFTER).unwrap_or(false))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}
