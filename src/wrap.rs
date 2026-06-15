//! `ctxforge wrap -- <command>` — run a read-only shell command transparently,
//! offload large stdout to the reversible store, and return a head+tail preview
//! plus a `retrieve_ref`. Small output passes through verbatim (zero behavior
//! change). Exactly one `ops.log` record is written per invocation.
//!
//! Only stdout is transformed: stderr is inherited (streamed live to the caller,
//! lossless, preserving error ordering), and the child's exit code is preserved.
//! This is the CLI seam wired from `main.rs` by T0.

use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::obs::OpLog;
use crate::store::Store;

/// How much of a truncated stdout to keep at the head and at the tail.
const PREVIEW_SIDE: usize = 2048;

/// Default stdout byte threshold above which output is offloaded to the store.
const DEFAULT_MAX_INLINE: usize = 8192;

/// Outcome of running a wrapped command: what to print, where the full blob
/// lives (if offloaded), how big the raw stdout was, and the child's exit code.
/// Returned by [`wrap_run`] so callers (and tests) can inspect behavior without
/// a real `process::exit`. `run_cli` only needs `printed`/`exit_code`;
/// `store_ref`/`raw_len` exist for the unit tests' assertions.
#[cfg_attr(not(test), allow(dead_code))]
struct WrapOutcome {
    printed: String,
    store_ref: Option<String>,
    raw_len: usize,
    exit_code: i32,
}

/// CLI entry: `args` is everything after `wrap` (expects `-- <command> [args…]`).
///
/// Parses the command, runs it via [`wrap_run`], prints the (possibly previewed)
/// stdout verbatim, then exits with the child's exit code.
pub fn run_cli(args: &[String]) -> Result<()> {
    let parts = command_parts(args);
    if parts.is_empty() {
        eprintln!("ctxforge wrap: usage: ctxforge wrap -- <command> [args…]");
        std::process::exit(2);
    }
    // The routing layer passes the original command as ONE single-quoted arg, so
    // join round-trips it faithfully; manual multi-arg use is also supported.
    let command = parts.join(" ");

    let dir = crate::obs::data_dir();
    let store = Store::open(&dir).with_context(|| format!("opening store under {}", dir.display()))?;
    let ops = OpLog::open(&dir);
    let max_inline = std::env::var("CTXFORGE_MAX_INLINE")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_MAX_INLINE);

    let outcome = wrap_run(&command, &store, &ops, max_inline)?;

    // Print exactly the child's stdout when small (zero behavior change), or the
    // head+tail preview when offloaded. stderr already streamed through inherit.
    print!("{}", outcome.printed);
    use std::io::Write as _;
    let _ = std::io::stdout().flush();

    std::process::exit(outcome.exit_code);
}

/// Split `args` at the `--` separator and return the command parts (everything
/// after it). With no `--`, all args are command parts.
fn command_parts(args: &[String]) -> Vec<String> {
    match args.iter().position(|a| a == "--") {
        Some(i) => args[i + 1..].to_vec(),
        None => args.to_vec(),
    }
}

/// Run `command` through `sh -c`, capturing its stdout fully while inheriting
/// stdin (so upstream pipes still feed the child) and stderr (streamed live).
/// Offload stdout larger than `max_inline` to `store`, returning a preview + ref;
/// otherwise pass it through verbatim. Writes exactly one `ops.log` record.
/// Does not print or exit — the outcome is returned for the caller to act on.
fn wrap_run(command: &str, store: &Store, ops: &OpLog, max_inline: usize) -> Result<WrapOutcome> {
    let input_summary = serde_json::json!({
        "cmd": truncate_chars(command, 200),
        "exit_code": serde_json::Value::Null,
    });
    let op = ops.start("bash_wrap", input_summary);

    // Run via a shell to preserve pipes/globs/redirects. stdin inherited so an
    // upstream pipe still feeds the child; stderr inherited so errors stream live
    // to the caller in order; stdout piped so we can capture it fully.
    let child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();

    let output = match child {
        Ok(child) => match child.wait_with_output() {
            Ok(out) => out,
            Err(e) => {
                // Spawned but could not be waited on / captured.
                op.finish(0, 0, None, "error", format!("wait failed: {e}"), None);
                return Err(anyhow::anyhow!("waiting on `sh -c`: {e}"));
            }
        },
        Err(e) => {
            // Could not spawn the shell at all.
            op.finish(0, 0, None, "error", format!("spawn failed: {e}"), None);
            return Err(anyhow::anyhow!("spawning `sh -c`: {e}"));
        }
    };

    // Signal death (no code) is treated as -1, matching the sandbox.
    let exit_code = output.status.code().unwrap_or(-1);

    // Lossy preview only at the boundary; the full blob is always stored.
    let full = String::from_utf8_lossy(&output.stdout).into_owned();
    let raw_len = full.len();

    let (printed, store_ref, note) = if raw_len > max_inline {
        let r = store
            .put(&full)
            .with_context(|| "storing wrapped stdout")?;
        let preview = make_preview(&full, &r);
        let note = format!("offloaded {raw_len} bytes");
        (preview, Some(r), note)
    } else {
        (full, None, "passthrough (small output)".to_string())
    };

    let printed_len = printed.len();
    op.finish(
        raw_len as u64,
        printed_len as u64,
        store_ref.clone(),
        "ok",
        note,
        None,
    );

    Ok(WrapOutcome {
        printed,
        store_ref,
        raw_len,
        exit_code,
    })
}

/// Build a head+tail preview of an oversized output, keeping char boundaries.
/// The footer names the ref and how to recover the full blob.
fn make_preview(full: &str, reference: &str) -> String {
    let head_end = floor_char_boundary(full, PREVIEW_SIDE);
    let tail_start = ceil_char_boundary(full, full.len().saturating_sub(PREVIEW_SIDE));
    let omitted = tail_start.saturating_sub(head_end);
    format!(
        "{}\n... [{} bytes omitted; full output: ctx_retrieve ref={}  (or: ctxforge verify {})] ...\n{}",
        &full[..head_end],
        omitted,
        reference,
        reference,
        &full[tail_start..]
    )
}

/// Truncate a string to at most `max` chars (char-boundary safe), appending an
/// ellipsis marker when it was cut. Used to keep the ops.log `cmd` summary small.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixtures() -> (tempfile::TempDir, Store, OpLog) {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let ops = OpLog::open(dir.path());
        (dir, store, ops)
    }

    #[test]
    fn small_output_passes_verbatim() {
        let (_dir, store, ops) = fixtures();
        let out = wrap_run("printf 'hello'", &store, &ops, 8192).unwrap();
        assert_eq!(out.printed, "hello");
        assert!(out.store_ref.is_none());
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.raw_len, 5);
    }

    #[test]
    fn large_output_is_offloaded_and_retrievable() {
        let (dir, store, ops) = fixtures();
        // Portable bash stdout generator: 50000 'A's, no python dependency.
        let out = wrap_run(
            r#"head -c 50000 /dev/zero | tr '\0' A"#,
            &store,
            &ops,
            8192,
        )
        .unwrap();

        // Confirm it actually offloaded.
        assert!(out.raw_len > 8192, "raw_len={} should exceed threshold", out.raw_len);
        let reference = out.store_ref.clone().expect("large output should have a ref");

        // The full body is stored and recoverable, and the preview is smaller.
        let full = store.get(&reference).unwrap().unwrap();
        assert!(full.contains(&"A".repeat(50000)));
        assert!(out.printed.len() < out.raw_len);
        assert!(out.printed.contains(&reference));

        // ops.log got exactly one record for this op.
        let log = std::fs::read_to_string(dir.path().join("ops.log")).unwrap();
        let lines: Vec<&str> = log.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("bash_wrap"));
    }

    #[test]
    fn exit_code_is_preserved() {
        let (_dir, store, ops) = fixtures();
        let out = wrap_run("exit 7", &store, &ops, 8192).unwrap();
        assert_eq!(out.exit_code, 7);
    }

    #[test]
    fn offloaded_blob_is_byte_for_byte_roundtrip() {
        let (_dir, store, ops) = fixtures();
        // Mixed content so a byte-exact roundtrip is meaningful, padded past the
        // threshold so it offloads.
        let out = wrap_run(
            r#"printf 'line one\nline two\n'; head -c 50000 /dev/zero | tr '\0' B"#,
            &store,
            &ops,
            8192,
        )
        .unwrap();
        let reference = out.store_ref.clone().expect("should offload");
        let stored = store.get(&reference).unwrap().unwrap();
        // The stored blob is exactly the (lossy-decoded) full child stdout.
        let mut expected = String::from("line one\nline two\n");
        expected.push_str(&"B".repeat(50000));
        assert_eq!(stored, expected);
    }
}
