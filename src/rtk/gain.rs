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
/// `tokens_saved_est` is `Δtotal_saved`. **T2 implements.**
pub fn sync() -> Result<()> {
    bail!("ctxforge rtk sync: not yet implemented (T2)")
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
}
