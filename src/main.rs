//! lens entrypoint.
//!
//! Two modes:
//!   * No subcommand → run the MCP stdio server (the default).
//!   * `lens hook <platform> <event>` (claude | opencode) → a short-lived session-continuity
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
        Some("doctor") => return lens::setup::run_doctor_cli(&args[2..]),
        // Hidden: refresh the cached latest-release tag for the SessionStart nudge.
        // Spawned detached by the hook; always silent, never errors out.
        Some("__update-check") => {
            lens::setup::run_update_check_cli();
            return Ok(());
        }
        // Hidden: the detached, throttled builder the MCP server hands a cold repo
        // to (`lens __build <root> <data_dir>`). Spawned with null stdio, never
        // typed, so it stays out of `print_usage`. The body lives in the library so
        // it can reuse the server's cross-process build lock rather than growing a
        // second implementation of the same lock semantics here.
        Some("__build") => return lens::warmup::run_build_cli(&args[2..]),
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
        Some("off") => {
            lens::disabled::run_off_cli(&args[2..]);
            return Ok(());
        }
        Some("on") => {
            lens::disabled::run_on_cli(&args[2..]);
            return Ok(());
        }
        Some("status") => {
            lens::status_cli::run_cli(&args[2..]);
            return Ok(());
        }
        Some("clean") => {
            lens::clean_cli::run_cli(&args[2..]);
            return Ok(());
        }
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
    println!("    lens off [path]             disable indexing for a tree");
    println!("    lens on [path]              re-enable a disabled tree");
    println!("    lens status [path]          show project state and data storage");
    println!("    lens clean [--all] [--yes]  remove orphaned and probe data");
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

    // Capture the data dir and scope verdict before `serve` consumes the forge,
    // so we can tidy the SQLite WAL sidecars on a clean shutdown below and gate
    // the heartbeat on the same root the server actually resolved.
    let data_dir = forge.data_dir().to_path_buf();
    let scoped = forge.scoped();

    // Liveness heartbeat for the routing layer's MCP-ready guard (a separate
    // hook process): it treats the server as reachable while ANY file under
    // `<data_dir>/heartbeats/` is fresh. Each server process owns exactly one
    // file, named after its own pid, so two servers sharing a data dir (two
    // sessions in the same repo) never touch each other's liveness signal —
    // there's no shared mutable state to race on, by construction. Re-touched
    // periodically so a crashed server's file goes stale and routing falls
    // back to passthrough.
    //
    // Unscoped roots get no heartbeat at all: no file, no `create_dir_all`, no
    // ticker task. Consequence: `routing::mcp_ready` reports not-ready for an
    // unscoped root and the hook-side rails fall back to passthrough — that's
    // the intended posture for a root lens has decided not to touch, not a
    // bug to fix here.
    let pidfile = if scoped { heartbeat_path(&data_dir) } else { None };
    if let Some(p) = &pidfile {
        if let Some(dir) = p.parent() {
            prune_stale_heartbeats(dir, p);
        }
        write_heartbeat(p);
        let _ = HEARTBEAT_FILE.set(p.clone());
        let p = p.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tick.tick().await;
                write_heartbeat(&p);
            }
        });
    }

    // stdin goes in wrapped so the client's disconnect is observed the instant it
    // happens rather than whenever the handler in flight happens to finish — see
    // `arm_disconnect_watchdog`.
    let (stdin, stdout) = stdio();
    let service = forge.serve((WatchDisconnect::new(stdin), stdout)).await?;
    let quit = service.waiting().await;
    // The clean shutdown path has begun, so the watchdog stands down. Reaching here
    // at all means rmcp drained every in-flight handler — its drain ends only once
    // each has dropped its response sender — so no build is running, which is
    // exactly the precondition `finalize_wal_files` needs. That is now structural:
    // the one path that used to reach the checkpoint with a build still writing to
    // the same databases was the 5s drain *timeout*, and the watchdog's grace
    // expires long before it.
    SHUTDOWN_STARTED.store(true, std::sync::atomic::Ordering::SeqCst);
    quit?;
    if let Some(p) = &pidfile {
        // Safe unconditionally: the filename is our own pid, so no other lens
        // process can be relying on this exact path for its own liveness.
        let _ = std::fs::remove_file(p);
    }
    finalize_wal_files(&data_dir);
    Ok(())
}

/// Grace between the client closing stdin and a forced exit. Generous for the clean
/// shutdown above to *start* (with nothing in flight `waiting()` returns in
/// single-digit milliseconds), short enough that a disconnect mid-build never leaves
/// the user with a CPU-burning orphan.
const DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_millis(750);

/// Set once the clean shutdown path has begun; the watchdog then stands down.
static SHUTDOWN_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// This process's own heartbeat file, published for the watchdog thread — it exits
/// the process from outside `run_server`'s scope, so it cannot capture the path.
static HEARTBEAT_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

/// Arm the disconnect watchdog: the client has closed its end of stdin, so nothing
/// more can be asked of us and the only question left is how fast we go away.
///
/// Why a forced exit rather than just the graceful path. On EOF rmcp breaks its
/// serve loop at once but then *drains* in-flight handler responses for up to five
/// seconds before `waiting()` returns; after that, dropping the tokio runtime blocks
/// until the worker thread running the handler is free. Handlers build the index
/// synchronously, so a client that leaves mid-build keeps a multi-core build alive
/// long after anyone can read its result — measured 8.7-9.7s on a 109MB tree, and
/// minutes on a Go-sized one. That is the process users end up killing by hand.
///
/// A plain OS thread rather than a tokio task, because the runtime is exactly what
/// may be wedged, and `std::process::exit` because it is the only exit that does not
/// first wait for the very worker threads that are busy.
///
/// Dying mid-build is safe, and is the same event as the SIGKILL the user would
/// otherwise deliver by hand: Tantivy publishes a segment only through its atomic
/// meta commit, SQLite runs in WAL mode, the staleness manifest is written last (so
/// a torn build reads as stale and is simply rebuilt on the next call), and
/// `build.pid` is reclaimed by the next session on a dead-pid check rather than
/// trusted. Skipping `finalize_wal_files` on this path is the documented SIGKILL
/// posture, not a new hazard — the sidecars fold in on the next clean shutdown.
fn arm_disconnect_watchdog() {
    std::thread::spawn(|| {
        std::thread::sleep(DISCONNECT_GRACE);
        if SHUTDOWN_STARTED.load(std::sync::atomic::Ordering::SeqCst) {
            return; // nothing was in flight; let the clean path finish on its own
        }
        // Ours and only ours — the file is named after our pid — so `routing::
        // mcp_ready` cannot read a server we are about to kill as reachable.
        if let Some(p) = HEARTBEAT_FILE.get() {
            let _ = std::fs::remove_file(p);
        }
        tracing::info!("client disconnected with work in flight; exiting");
        std::process::exit(0);
    });
}

/// stdin wrapped so the moment the client closes its end is visible here. rmcp owns
/// the transport and offers no disconnect hook of its own — its signal, `waiting()`
/// returning, is the very thing that arrives late — so EOF is caught at the one
/// place it is still on time.
struct WatchDisconnect<R> {
    inner: R,
    armed: bool,
}

impl<R> WatchDisconnect<R> {
    fn new(inner: R) -> Self {
        WatchDisconnect {
            inner,
            armed: false,
        }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for WatchDisconnect<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        // A zero-capacity read reads nothing for reasons that have nothing to do
        // with the peer, so it must never be mistaken for a disconnect.
        let had_room = buf.remaining() > 0;
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        // A read error is a disconnect too: a torn pipe surfaces as `Err`, not EOF,
        // and rmcp shuts down through the same drain that arrives late either way.
        let eof = matches!(polled, std::task::Poll::Ready(Err(_)))
            || (had_room
                && matches!(polled, std::task::Poll::Ready(Ok(())))
                && buf.filled().len() == before);
        if eof && !self.armed {
            self.armed = true;
            arm_disconnect_watchdog();
        }
        polled
    }
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

/// Resolve this process's own heartbeat file, `<data_dir>/heartbeats/<pid>.pid`,
/// under the `Forge`'s own resolved data dir rather than re-deriving one from
/// `$LENS_DIR`/`current_dir()` — `data_dir()` is the single source of truth for
/// where this `Forge` instance's on-disk state lives, so the heartbeat always
/// matches the root the server actually indexes. `None` if unresolvable. Naming
/// the file after the pid — rather than a single shared `server.pid` — means
/// concurrent lens servers in the same data dir each own a distinct path: no
/// process ever writes or deletes another's liveness file. Caller is expected
/// to only invoke this for a scoped root.
fn heartbeat_path(data_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let dir = data_dir.join("heartbeats");
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
