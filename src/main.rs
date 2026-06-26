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
        Some("watch") => return lens::warmup::run_watch_cli(&args[2..]),
        _ => {}
    }
    // `--explain` is an alias for LENS_EXPLAIN=1 (opt-in per-op trace).
    if args.iter().any(|a| a == "--explain") {
        std::env::set_var("LENS_EXPLAIN", "1");
    }
    run_server()
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
    // hook process): it treats this server as reachable only while
    // `<data_dir>/server.pid` is fresh. Re-touched periodically so a crashed
    // server goes stale and routing falls back to passthrough.
    let pidfile = heartbeat_path();
    if let Some(p) = &pidfile {
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

    let service = forge.serve(stdio()).await?;
    service.waiting().await?;
    if let Some(p) = &pidfile {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

/// Resolve `<data_dir>/server.pid`, matching how the server/hook resolve the
/// data dir (`$LENS_DIR`, else `<cwd>/.lens`). `None` if unresolvable.
fn heartbeat_path() -> Option<std::path::PathBuf> {
    let dir = match std::env::var_os("LENS_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir().ok()?.join(".lens"),
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("server.pid"))
}

/// Best-effort write of the current pid; updates mtime so freshness checks pass.
fn write_heartbeat(path: &std::path::Path) {
    let _ = std::fs::write(path, std::process::id().to_string());
}
