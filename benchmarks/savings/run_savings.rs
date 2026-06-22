//! Savings benchmark runner.
//!
//! Pushes each workload archetype through the real ctxforge tools and prints the
//! savings table (headroom-style, plus raw bytes and the naive-agent baseline),
//! segmented by which mechanism produced the saving.
//!
//!   cargo run --bin bench_savings            # print the table
//!   cargo run --bin bench_savings -- --update  # also rewrite the committed baseline
//!
//! `expected/savings.json` is the committed baseline; the `#[cfg(test)]`
//! regression guard below fails if a code change moves any number beyond a
//! tolerance.

#[path = "../common/savings.rs"]
#[allow(dead_code)] // scale-curve helpers in the shared module are used by other bins
mod savings;

use std::path::PathBuf;

use savings::{bench_root, compute_savings, render_savings_markdown};

fn expected_path() -> PathBuf {
    bench_root().join("savings/expected/savings.json")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let update = std::env::args().any(|a| a == "--update");

    let rows = compute_savings().await?;
    println!("# ctxforge savings benchmark\n");
    print!("{}", render_savings_markdown(&rows));

    if update {
        let json = serde_json::to_string_pretty(&rows)?;
        let path = expected_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, json + "\n")?;
        eprintln!("\nupdated baseline: {}", path.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::savings::SavingsRow;

    fn load_expected() -> Vec<SavingsRow> {
        let raw = std::fs::read_to_string(expected_path()).expect(
            "expected/savings.json missing; run `cargo run --bin bench_savings -- --update`",
        );
        serde_json::from_str(&raw).expect("expected/savings.json is not valid JSON")
    }

    // Regression guard: deterministic fixtures mean before-bytes are exact and
    // after-bytes are stable. Allow a small tolerance so a different grep build
    // (BSD vs GNU) doesn't trip the guard, and flag any savings drift > 5 points.
    #[tokio::test]
    async fn savings_match_committed_baseline() {
        let expected = load_expected();
        let actual = compute_savings().await.expect("compute_savings failed");
        assert_eq!(
            actual.len(),
            expected.len(),
            "row count changed vs baseline"
        );
        for (a, e) in actual.iter().zip(expected.iter()) {
            assert_eq!(a.workload, e.workload, "workload label changed");
            assert_eq!(
                a.mechanism, e.mechanism,
                "mechanism changed for {}",
                a.workload
            );

            // before is from committed fixtures -> must be exact.
            assert_eq!(
                a.before_bytes, e.before_bytes,
                "before_bytes drifted for {}",
                a.workload
            );

            // after within 10% of baseline.
            let lo = (e.after_bytes as f64 * 0.90) as usize;
            let hi = (e.after_bytes as f64 * 1.10) as usize + 4;
            assert!(
                a.after_bytes >= lo && a.after_bytes <= hi,
                "after_bytes for {} = {} outside [{}, {}] (baseline {})",
                a.workload,
                a.after_bytes,
                lo,
                hi,
                e.after_bytes
            );

            let drift = (a.savings_pct as i64 - e.savings_pct as i64).abs();
            assert!(
                drift <= 5,
                "savings for {} moved {} points (now {}%, baseline {}%) — investigate",
                a.workload,
                drift,
                a.savings_pct,
                e.savings_pct
            );
        }
    }

    // Every mechanism must actually save (after < before), or the row is a
    // strawman / a regression.
    #[tokio::test]
    async fn every_workload_saves() {
        let rows = compute_savings().await.expect("compute_savings failed");
        for r in &rows {
            assert!(
                r.after_bytes < r.before_bytes,
                "{} did not save: before {} after {}",
                r.workload,
                r.before_bytes,
                r.after_bytes
            );
        }
    }
}
