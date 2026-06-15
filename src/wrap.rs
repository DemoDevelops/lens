//! `ctxforge wrap -- <command>` — run a read-only shell command, offload large
//! stdout to the reversible store, and return a head+tail preview + retrieve_ref.
//!
//! Implemented in T2; this is the CLI seam wired from `main.rs` by T0.

/// CLI entry: `args` is everything after `wrap` (expects `-- <command> [args…]`).
/// Stub: prints usage and exits 0 until T2 lands the real implementation.
pub fn run_cli(_args: &[String]) -> anyhow::Result<()> {
    println!("ctxforge wrap: usage: ctxforge wrap -- <command> [args…]");
    Ok(())
}
