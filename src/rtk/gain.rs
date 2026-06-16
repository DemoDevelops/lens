//! `rtk gain` bridge — read RTK's *own* measured shell-command savings and (T2)
//! reconcile them into the ctxforge op log as `rtk_shell` records.
//!
//! **T0 scaffold provides** the JSON parse ([`parse_gain`], proven against the
//! captured sample) and a working [`read_gain`] (runs `rtk gain --format json`),
//! so the dashboard/stats surfacing (T3) can be tested against a stub `rtk`.
//! **T2 implements** [`sync`]: diff `rtk gain` against a watermark and append an
//! `rtk_shell` `OpRecord` carrying `tokens_saved_est = Δtotal_saved` (RTK's own
//! number — never re-estimated). See `RTK_NOTES.md` §4/§9.

use anyhow::{bail, Context, Result};

use super::GainSummary;

/// Reporting scope for `rtk gain`. `Project` maps to RTK's `--project`, which
/// scopes to the current working directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

/// Parse `rtk gain --format json` stdout into a [`GainSummary`].
pub fn parse_gain(json: &str) -> Result<GainSummary> {
    serde_json::from_str(json).context("parsing `rtk gain --format json` output")
}

/// Read RTK's measured savings by running `rtk gain --format json [--project]`.
///
/// Returns `Err` if RTK isn't installed or the call fails — callers that surface
/// this (stats/dashboard) treat it best-effort and degrade to "no RTK block".
pub fn read_gain(scope: Scope) -> Result<GainSummary> {
    let mut args = vec!["gain", "--format", "json"];
    if scope == Scope::Project {
        args.push("--project");
    }
    let out = super::run_rtk(&args)?;
    if !out.status.success() {
        bail!(
            "`rtk gain` exited unsuccessfully ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    parse_gain(&String::from_utf8_lossy(&out.stdout))
}

/// `ctxforge rtk sync` — diff `rtk gain` against the watermark at
/// `$CTXFORGE_DIR/rtk_watermark.json` and append an `rtk_shell` op record whose
/// `tokens_saved_est` is `Δtotal_saved` (RTK's own number, never re-estimated).
///
/// No-op when RTK isn't installed. When there's no new activity since the last
/// sync the watermark is still refreshed but **no** record is appended, so the
/// op log never double-counts a cumulative total. See `RTK_NOTES.md` §4.
pub fn sync() -> Result<()> {
    let data_dir = crate::obs::data_dir();

    if !super::rtk_available() {
        println!("rtk not installed; nothing to sync");
        return Ok(());
    }

    // RTK's own measured total (global scope).
    let cur = read_gain(Scope::Global)?;

    // Watermark = the last-seen cumulative figure; absent/garbled ⇒ zeros.
    let watermark = data_dir.join("rtk_watermark.json");
    let prev = std::fs::read_to_string(&watermark)
        .ok()
        .and_then(|raw| parse_gain(&raw).ok())
        .unwrap_or_default();

    let delta_saved: i64 = cur.summary.total_saved as i64 - prev.summary.total_saved as i64;
    let delta_commands: i64 = cur.summary.total_commands as i64 - prev.summary.total_commands as i64;

    if delta_commands <= 0 && delta_saved <= 0 {
        // Nothing new — refresh the watermark (it's authoritative) but don't
        // append a record, so re-running sync can't re-bank the same savings.
        write_watermark(&watermark, &cur)?;
        println!("no new rtk activity since last sync");
        return Ok(());
    }

    let pid = std::process::id();
    let rec = crate::obs::OpRecord {
        ts: crate::obs::iso8601_now(),
        session_id: std::env::var("CTXFORGE_SESSION_ID").ok(),
        agent_id: std::env::var("CTXFORGE_AGENT_ID").unwrap_or_else(|_| format!("pid-{pid}")),
        pid,
        tool: "rtk_shell".into(),
        input_summary: serde_json::json!({
            "commands": delta_commands,
            "total_input": cur.summary.total_input,
            "total_output": cur.summary.total_output,
            "scope": "global",
        }),
        raw_bytes_in: 0,
        bytes_returned: 0,
        // RTK's OWN delta — do not divide by 4 / re-estimate.
        tokens_saved_est: delta_saved,
        store_ref: None,
        duration_ms: 0,
        lock_wait_ms: 0,
        outcome: "ok".into(),
        note: format!(
            "rtk gain Δ +{delta_saved} tokens over {delta_commands} commands; \
             cumulative total_saved={}",
            cur.summary.total_saved
        ),
    };
    crate::obs::OpLog::open(&data_dir).append(&rec);

    write_watermark(&watermark, &cur)?;
    println!(
        "rtk sync: +{delta_saved} tokens over {delta_commands} commands \
         (cumulative total_saved={})",
        cur.summary.total_saved
    );
    Ok(())
}

/// Persist `cur` as the new watermark (pretty JSON, so it's human-diffable).
fn write_watermark(path: &std::path::Path, cur: &GainSummary) -> Result<()> {
    let json = serde_json::to_string_pretty(cur).context("serializing rtk watermark")?;
    std::fs::write(path, json)
        .with_context(|| format!("writing rtk watermark to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured verbatim from `rtk 0.28.2` on this machine (RTK_NOTES.md §4),
    /// `rtk gain --format json`, global scope. The interface contract: `GainSummary`
    /// must deserialize this exact output.
    const SAMPLE_GLOBAL: &str = r#"{
  "summary": {
    "total_commands": 3753,
    "total_input": 3689788,
    "total_output": 1424127,
    "total_saved": 2268362,
    "avg_savings_pct": 61.47675693020845,
    "total_time_ms": 2990161,
    "avg_time_ms": 796
  }
}"#;

    #[test]
    fn gain_summary_deserializes_captured_sample() {
        let g = parse_gain(SAMPLE_GLOBAL).expect("parse captured rtk gain json");
        assert_eq!(g.summary.total_commands, 3753);
        assert_eq!(g.summary.total_input, 3_689_788);
        assert_eq!(g.summary.total_output, 1_424_127);
        assert_eq!(g.summary.total_saved, 2_268_362);
        assert_eq!(g.summary.total_time_ms, 2_990_161);
        assert_eq!(g.summary.avg_time_ms, 796);
        assert!((g.summary.avg_savings_pct - 61.476_756_930_208_45).abs() < 1e-9);
        // No period breakdowns without a --daily/--weekly/--monthly/--all flag.
        assert!(g.daily.is_none() && g.weekly.is_none() && g.monthly.is_none());
    }

    #[test]
    fn parse_tolerates_period_blocks() {
        // With --all, RTK emits daily/weekly/monthly arrays; the parse must accept them.
        let with_periods = r#"{"summary":{"total_commands":1,"total_input":10,
            "total_output":2,"total_saved":8,"avg_savings_pct":80.0,
            "total_time_ms":5,"avg_time_ms":5},"daily":[],"weekly":[],"monthly":[]}"#;
        let g = parse_gain(with_periods).unwrap();
        assert_eq!(g.summary.total_saved, 8);
        assert!(g.daily.is_some());
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_gain("not json").is_err());
    }

    // ── sync() offline tests (network-free; stub `rtk`) ────────────────────────
    //
    // `sync()` reads its data dir from `crate::obs::data_dir()` (env `CTXFORGE_DIR`)
    // and resolves the stub `rtk` from `CTXFORGE_HOME` — both PROCESS-global. The
    // `session::hook` unit tests read `CTXFORGE_DIR` too (via `resolve_data_dir`),
    // and `cargo test` runs in parallel with no `serial_test` crate available, so
    // mutating those vars in-process would corrupt sibling tests. The project's own
    // convention (see `tests/*.rs`) is to scope `CTXFORGE_DIR` to a CHILD process's
    // env, never the test process. `CARGO_BIN_EXE_ctxforge` isn't injected for
    // `--lib` unit tests, so we re-exec THIS test binary instead: the parent spawns
    // the `#[ignore]`d `sync_child` with the env scoped to the child, and the child
    // runs the real A→B→C against the stub. No env leaks into the parent process.

    /// Write an executable stub `<home>/bin/rtk` that prints `rtk 0.28.2` for
    /// `--version` and a `GainSummary` with the given `total_saved`/`total_commands`
    /// for `gain --format json` (tolerating a trailing `--project`). Mirrors a
    /// minimal `rtk gain` so resolution (managed-first) lands on the stub.
    #[cfg(unix)]
    fn write_stub_rtk(home: &std::path::Path, total_saved: u64, total_commands: u64) {
        use std::os::unix::fs::PermissionsExt;
        let bin = home.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // total_input/total_output are arbitrary but stable; sync stashes them in
        // input_summary so the test can assert the byte planes stay clean.
        let json = format!(
            r#"{{"summary":{{"total_commands":{total_commands},"total_input":1000,"total_output":400,"total_saved":{total_saved},"avg_savings_pct":60.0,"total_time_ms":5,"avg_time_ms":5}}}}"#
        );
        // The stub echoes the canned JSON for `gain`; `--project` is ignored.
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n  \
             --version) echo 'rtk 0.28.2' ;;\n  \
             gain) echo '{json}' ;;\n  \
             *) echo 'rtk: unexpected args' >&2; exit 2 ;;\n\
             esac\n"
        );
        let p = bin.join("rtk");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Count `rtk_shell` lines in `<datadir>/ops.log` and return the last such
    /// record (parsed), if any.
    fn rtk_shell_lines(datadir: &std::path::Path) -> (usize, Option<crate::obs::OpRecord>) {
        let raw = std::fs::read_to_string(datadir.join("ops.log")).unwrap_or_default();
        let mut count = 0;
        let mut last = None;
        for line in raw.lines() {
            if let Ok(rec) = serde_json::from_str::<crate::obs::OpRecord>(line) {
                if rec.tool == "rtk_shell" {
                    count += 1;
                    last = Some(rec);
                }
            }
        }
        (count, last)
    }

    /// Parent: spin up tempdirs for the stub home + data dir, then re-exec this
    /// test binary to run [`sync_child`] with `CTXFORGE_HOME`/`CTXFORGE_DIR` scoped
    /// to the CHILD only (process isolation ⇒ no leak into sibling unit tests).
    #[cfg(unix)]
    #[test]
    fn sync_banks_rtk_delta_against_watermark() {
        let home = tempfile::tempdir().unwrap();
        let datadir = tempfile::tempdir().unwrap();

        let exe = std::env::current_exe().expect("current test exe");
        let status = std::process::Command::new(&exe)
            .args(["--exact", "--ignored", "--nocapture", "rtk::gain::tests::sync_child"])
            .env("CTXFORGE_HOME", home.path()) // stub rtk resolves here
            .env("CTXFORGE_DIR", datadir.path()) // ops.log + watermark land here
            // Keep the child deterministic regardless of the outer env.
            .env_remove("CTXFORGE_AGENT_ID")
            .env_remove("CTXFORGE_SESSION_ID")
            .status()
            .expect("spawn sync_child");
        assert!(status.success(), "sync_child failed: {status}");

        // Belt-and-braces: re-verify the child's side effects from the parent.
        let (count, last) = rtk_shell_lines(datadir.path());
        assert_eq!(count, 2, "exactly two rtk_shell lines after A+C (B banks nothing)");
        assert_eq!(
            last.unwrap().tokens_saved_est,
            750,
            "final rtk_shell tokens_saved_est == Δtotal_saved of phase C"
        );
        assert!(
            datadir.path().join("rtk_watermark.json").exists(),
            "watermark persisted"
        );
    }

    /// Child: the real A→B→C, run in its own process with `CTXFORGE_*` set by the
    /// parent. `#[ignore]` so the normal `cargo test` run never executes it directly.
    #[cfg(unix)]
    #[test]
    #[ignore = "spawned by sync_banks_rtk_delta_against_watermark with scoped env"]
    fn sync_child() {
        let home = std::path::PathBuf::from(std::env::var_os("CTXFORGE_HOME").unwrap());
        let datadir = std::path::PathBuf::from(std::env::var_os("CTXFORGE_DIR").unwrap());

        // ── A: first sync banks the full cumulative total as the delta ──────────
        write_stub_rtk(&home, 1000, 50);
        sync().expect("first sync");
        let (count, last) = rtk_shell_lines(&datadir);
        assert_eq!(count, 1, "A: exactly one rtk_shell line after first sync");
        let rec = last.unwrap();
        assert_eq!(rec.tokens_saved_est, 1000, "A: Δtotal_saved == stub total_saved");
        assert_eq!(rec.raw_bytes_in, 0, "A: byte planes stay clean");
        assert_eq!(rec.bytes_returned, 0, "A: byte planes stay clean");
        assert!(datadir.join("rtk_watermark.json").exists(), "A: watermark created");

        // ── B: re-sync with identical stub output appends NOTHING ───────────────
        sync().expect("second sync (no new activity)");
        let (count, _) = rtk_shell_lines(&datadir);
        assert_eq!(count, 1, "B: watermark holds — no new rtk_shell line");

        // ── C: bump the stub's total_saved; next sync banks only the increment ──
        write_stub_rtk(&home, 1750, 60); // +750 saved over +10 commands
        sync().expect("third sync (new activity)");
        let (count, last) = rtk_shell_lines(&datadir);
        assert_eq!(count, 2, "C: one new rtk_shell line for the increment");
        let rec = last.unwrap();
        assert_eq!(rec.tokens_saved_est, 750, "C: tokens_saved_est == Δtotal_saved");
        assert_eq!(
            rec.input_summary.get("commands").and_then(|v| v.as_i64()),
            Some(10),
            "C: input_summary.commands == Δtotal_commands"
        );
    }
}
