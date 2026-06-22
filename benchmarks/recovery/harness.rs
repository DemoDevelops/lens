//! Session-recovery benchmark harness.
//!
//! Runs every scenario in `scenarios/*.json` through three isolated arms
//! (no-continuity floor, Context Mode bar, ctxforge candidate), scores whether
//! the working state survived a compaction boundary, and emits the recovery
//! table segmented by scenario set.
//!
//!   cargo run --bin bench_recovery        # real model if ANTHROPIC_API_KEY set, else mock
//!
//! The Context Mode arm runs its real hook scripts (requires `bun` + the
//! context-mode plugin, or `CONTEXT_MODE_HOOKS_DIR`); when unavailable it is
//! reported as such rather than faked.

#[path = "../common/recovery.rs"]
mod recovery;

use std::path::PathBuf;

use recovery::{
    aggregate, default_model, load_scenarios, render_recovery_markdown, run_scenario, Model,
    ScenarioResult,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `CTXFORGE_BENCH_BACKEND=claude-pty` bills to plan quota via interactive
    // Claude Code; otherwise Anthropic API key > mock.
    let backend = std::env::var("CTXFORGE_BENCH_BACKEND").unwrap_or_default();
    let has_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let (model, pending, mode) = if backend == "claude-pty" {
        eprintln!("running recovery harness via claude-pty (plan quota, tools disabled)");
        (Model::ClaudePty(default_model()), false, "real")
    } else if has_key {
        (Model::Anthropic(default_model()), false, "real")
    } else {
        eprintln!(
            "ANTHROPIC_API_KEY not set — running recovery harness in MOCK mode \
             (survival plumbing only, no real-model recovery). Set the key for a real run."
        );
        (Model::Mock, true, "mock")
    };

    let scenarios = load_scenarios()?;
    let mut results: Vec<ScenarioResult> = Vec::new();
    for s in &scenarios {
        match run_scenario(s, &model) {
            Ok(r) => results.push(r),
            Err(e) => eprintln!("scenario {} failed: {e}", s.id),
        }
    }

    let groups = aggregate(&results);
    println!("# ctxforge session-recovery benchmark\n");
    print!(
        "{}",
        render_recovery_markdown(&groups, &model.label(), pending)
    );

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/recovery/results");
    std::fs::create_dir_all(&out_dir)?;
    let payload = serde_json::json!({
        "mode": mode,
        "model": model.label(),
        "groups": groups,
        "scenarios": results,
    });
    let out_path = out_dir.join(format!("{mode}.json"));
    std::fs::write(&out_path, serde_json::to_string_pretty(&payload)? + "\n")?;
    eprintln!("\nwrote {}", out_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::recovery::*;
    use serde_json::json;

    #[test]
    fn survival_oracle_presence() {
        let gt = json!({"file": "src/auth.rs"});
        assert_eq!(
            mock_answer(
                "## Files Modified\n- src/auth.rs",
                &["src/auth.rs".into()],
                &gt
            ),
            gt
        );
        assert_eq!(
            mock_answer("nothing useful", &["src/auth.rs".into()], &gt),
            json!({"file": "UNKNOWN"})
        );
        // empty context (no-continuity floor) never survives.
        assert_eq!(
            mock_answer("", &["x".into()], &gt),
            json!({"file": "UNKNOWN"})
        );
    }

    /// Mock run over the real scenarios: exercises scenario loading, the
    /// ctxforge recovery pipeline, the no-continuity floor, scoring, and
    /// aggregation without API calls or the Context Mode subprocess.
    #[test]
    fn mock_run_ctxforge_beats_floor() {
        let scenarios = load_scenarios().expect("load scenarios");
        assert!(
            scenarios.len() >= 8,
            "expected >= 8 scenarios, got {}",
            scenarios.len()
        );

        let model = Model::Mock;
        let mut results = Vec::new();
        for s in &scenarios {
            // Score only the two always-available arms here so the test is
            // hermetic (Context Mode requires bun + the plugin).
            let cf = recover(s, Arm::Ctxforge).unwrap().unwrap();
            let nc = recover(s, Arm::NoContinuity).unwrap().unwrap();
            let cf_ok = score(
                &mock_answer(&cf, &s.evidence, &s.ground_truth),
                &s.ground_truth,
                &s.check,
            );
            let nc_ok = score(
                &mock_answer(&nc, &s.evidence, &s.ground_truth),
                &s.ground_truth,
                &s.check,
            );
            assert!(cf_ok, "ctxforge failed to recover scenario {}", s.id);
            assert!(
                !nc_ok,
                "no-continuity should fail scenario {} (floor)",
                s.id
            );
            results.push((s.set.clone(), cf_ok, nc_ok));
        }

        // Every scenario: ctxforge survives, floor does not.
        assert!(results.iter().all(|(_, cf, _)| *cf));
        assert!(results.iter().all(|(_, _, nc)| !*nc));

        // Full aggregation path works through run_scenario too (CM may be n/a).
        let full: Vec<_> = scenarios
            .iter()
            .map(|s| run_scenario(s, &model).unwrap())
            .collect();
        let groups = aggregate(&full);
        assert_eq!(
            groups.len(),
            2,
            "expected file_task + error_decision groups"
        );
        for g in &groups {
            assert!(
                g.ctxforge >= g.no_continuity,
                "{}: ctxforge below floor",
                g.set
            );
            assert!(
                (g.ctxforge - 1.0).abs() < f64::EPSILON,
                "{}: ctxforge should recover all",
                g.set
            );
        }
    }
}
