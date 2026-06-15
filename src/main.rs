//! ctxforge entrypoint.
//!
//! Two modes:
//!   * No subcommand → run the MCP stdio server (the default).
//!   * `ctxforge hook <platform> <event>` → a short-lived session-continuity
//!     lifecycle hook (stdin = hook payload, stdout = hook response).
//!   * `ctxforge session <install|uninstall|status>` → manage the hooks.
//!   * `ctxforge stats [...]` / `ctxforge verify [...]` → read-only observability
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

use ctxforge::obs;
use ctxforge::server::Forge;
use ctxforge::session;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("hook") => return session::hook::run_cli(&args[2..]),
        Some("session") => return session::install::run_cli(&args[2..]),
        Some("stats") => return obs::stats::run_cli(&args[2..]),
        Some("verify") => return obs::verify::run_cli(&args[2..]),
        Some("dashboard") => return obs::dashboard::run_cli(&args[2..]),
        _ => {}
    }
    // `--explain` is an alias for CTXFORGE_EXPLAIN=1 (opt-in per-op trace).
    if args.iter().any(|a| a == "--explain") {
        std::env::set_var("CTXFORGE_EXPLAIN", "1");
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
    tracing::info!("ctxforge starting on stdio");

    let service = forge.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
