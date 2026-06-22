//! Navigation benchmark runner.
//!
//! Measures the code graph's per-operation leverage on navigation questions
//! (definition lookup, who-calls, reachability) against a realistic naive
//! grep+read path — bytes into context and round trips, with correctness checked
//! against ground truth. Fully deterministic (no model): same committed fixture →
//! same numbers.
//!
//!   cargo run --bin bench_navigation             # print the table
//!   cargo run --bin bench_navigation -- --update # also rewrite the committed baseline
//!
//! `expected/navigation.json` is the committed baseline; the `#[cfg(test)]`
//! regression guard below fails if a code change moves any number beyond tolerance
//! or breaks an answer.

#[path = "../common/navigation.rs"]
mod navigation;

use std::path::PathBuf;

use navigation::{bench_root, compute_navigation, render_navigation_markdown};

fn expected_path() -> PathBuf {
    bench_root().join("navigation/expected/navigation.json")
}

fn main() -> anyhow::Result<()> {
    let update = std::env::args().any(|a| a == "--update");

    let rows = compute_navigation()?;
    println!("# ctxforge navigation benchmark\n");
    print!("{}", render_navigation_markdown(&rows));

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
    use crate::navigation::NavRow;

    fn load_expected() -> Vec<NavRow> {
        let raw = std::fs::read_to_string(expected_path()).expect(
            "expected/navigation.json missing; run `cargo run --bin bench_navigation -- --update`",
        );
        serde_json::from_str(&raw).expect("expected/navigation.json is not valid JSON")
    }

    // Regression guard: deterministic fixture means naive bytes and round trips are
    // exact; graph bytes may shift slightly if the serialized view changes, so allow
    // a small tolerance there.
    #[test]
    fn navigation_matches_committed_baseline() {
        let expected = load_expected();
        let actual = compute_navigation().expect("compute_navigation failed");
        assert_eq!(
            actual.len(),
            expected.len(),
            "row count changed vs baseline"
        );
        for (a, e) in actual.iter().zip(expected.iter()) {
            assert_eq!(a.id, e.id, "question id changed");
            assert_eq!(a.qtype, e.qtype, "qtype changed for {}", a.id);
            assert_eq!(
                a.naive_bytes, e.naive_bytes,
                "naive_bytes drifted for {}",
                a.id
            );
            assert_eq!(
                a.naive_round_trips, e.naive_round_trips,
                "naive_round_trips drifted for {}",
                a.id
            );
            assert_eq!(
                a.graph_round_trips, e.graph_round_trips,
                "graph_round_trips drifted for {}",
                a.id
            );
            let lo = (e.graph_bytes as f64 * 0.90) as usize;
            let hi = (e.graph_bytes as f64 * 1.10) as usize + 4;
            assert!(
                a.graph_bytes >= lo && a.graph_bytes <= hi,
                "graph_bytes for {} = {} outside [{}, {}] (baseline {})",
                a.id,
                a.graph_bytes,
                lo,
                hi,
                e.graph_bytes
            );
        }
    }

    // A fast-but-wrong graph is worthless: every question must answer correctly.
    #[test]
    fn graph_answers_every_question_correctly() {
        let rows = compute_navigation().expect("compute_navigation failed");
        for r in &rows {
            assert!(r.correct, "graph answered {} incorrectly", r.id);
        }
    }

    // The graph never costs more round trips, and strictly fewer on the relational
    // questions (who-calls, reachability) — the speed claim.
    #[test]
    fn graph_uses_fewer_round_trips_on_relational_questions() {
        let rows = compute_navigation().expect("compute_navigation failed");
        for r in &rows {
            assert!(
                r.graph_round_trips <= r.naive_round_trips,
                "{} graph round trips {} > naive {}",
                r.id,
                r.graph_round_trips,
                r.naive_round_trips
            );
            if r.qtype == "callers" || r.qtype == "path" {
                assert!(
                    r.graph_round_trips < r.naive_round_trips,
                    "{} should save round trips: graph {} vs naive {}",
                    r.id,
                    r.graph_round_trips,
                    r.naive_round_trips
                );
            }
        }
    }
}
