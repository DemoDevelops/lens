//! Deterministic claim proof.
//!
//! Runs the no-model claims from `FEATURES.md` against live numbers and asserts
//! each meets its *advertised threshold* (not just "matches last baseline"), prints
//! a PASS/FAIL table, and exits non-zero if any claim fails. This is the runnable
//! proof for the deterministic tier: byte savings, navigation leverage, and lossless
//! recovery. The model-dependent claims (accuracy, recovery, adoption) are evidenced
//! by `bench_accuracy` / `bench_recovery` / `bench_adoption`, not here.
//!
//!   cargo run --bin bench_prove   # prints the table; exit 0 iff every claim passes

#[path = "../common/navigation.rs"]
#[allow(dead_code)] // render helpers in the shared module are unused here
mod navigation;
#[path = "../common/savings.rs"]
#[allow(dead_code)] // scale-curve / render helpers in the shared module are unused here
mod savings;

use serde_json::Value;

use lens::store::{compress, Store};
use navigation::compute_navigation;
use savings::{bench_root, code_search_at, compute_savings, issue_triage_at};

struct Claim {
    name: String,
    threshold: String,
    measured: String,
    pass: bool,
}

fn pct(before: usize, after: usize) -> i64 {
    if before == 0 {
        return 0;
    }
    (((before as f64 - after as f64) / before as f64) * 100.0).round() as i64
}

/// Evaluate every deterministic claim against live numbers.
async fn compute_claims() -> anyhow::Result<Vec<Claim>> {
    let mut claims = Vec::new();

    // Darkroom: a buried-root-cause log stays out of context (grep in the darkroom).
    let rows = compute_savings().await?;
    let darkroom = rows
        .iter()
        .find(|r| r.mechanism == "darkroom")
        .expect("darkroom row");
    claims.push(Claim {
        name: "Darkroom keeps large output out of context".into(),
        threshold: "log-debug byte savings >= 90%".into(),
        measured: format!("{}%", darkroom.savings_pct),
        pass: darkroom.savings_pct >= 90,
    });

    // Index: at realistic session scale (1x is a 37% diagnostic fixture).
    let (cb, ca) = code_search_at(10);
    let cs = pct(cb, ca);
    claims.push(Claim {
        name: "Indexed search scales".into(),
        threshold: "code-search byte savings @10x >= 90%".into(),
        measured: format!("{cs}% ({cb} -> {ca} bytes)"),
        pass: cs >= 90,
    });

    // Compression: columnar schema-extraction on a structured payload.
    let (ib, ia) = issue_triage_at(10);
    let it = pct(ib, ia);
    claims.push(Claim {
        name: "Columnar compression shrinks structured payloads".into(),
        threshold: "issue-triage byte savings @10x >= 60%".into(),
        measured: format!("{it}% ({ib} -> {ia} bytes)"),
        pass: it >= 60,
    });

    // Compression is lossless: expand(compact(x)) == x.
    let issues_raw =
        std::fs::read_to_string(bench_root().join("savings/workloads/issue_triage/issues.json"))?;
    let issues: Value = serde_json::from_str(&issues_raw)?;
    let restored = compress::expand_json(&compress::compact_json(&issues));
    claims.push(Claim {
        name: "Compression is lossless".into(),
        threshold: "expand(compact(x)) == x".into(),
        measured: if restored == issues {
            "exact".into()
        } else {
            "MISMATCH".into()
        },
        pass: restored == issues,
    });

    // Offload is lossless: store.get(put(x)) == x, byte-for-byte.
    let data = tempfile::tempdir()?;
    let store = Store::open(&data.path().join(".lens"))?;
    let blob = issues_raw.repeat(4);
    let reference = store.put(&blob)?;
    let got = store.get(&reference)?.unwrap_or_default();
    claims.push(Claim {
        name: "Offloaded output recovers byte-for-byte".into(),
        threshold: "store.get(put(x)) == x".into(),
        measured: if got == blob {
            format!("{} bytes recovered", got.len())
        } else {
            "MISMATCH".into()
        },
        pass: got == blob,
    });

    // Graph: navigation answered correctly, with strictly fewer round trips and real
    // byte savings on the reachability (multi-hop) questions.
    let nav = compute_navigation()?;
    let correct = nav.iter().filter(|r| r.correct).count();
    let reach: Vec<_> = nav.iter().filter(|r| r.qtype == "path").collect();
    let rt_win = reach
        .iter()
        .all(|r| r.graph_round_trips < r.naive_round_trips);
    let nb: usize = reach.iter().map(|r| r.naive_bytes).sum();
    let gb: usize = reach.iter().map(|r| r.graph_bytes).sum();
    let reach_saved = pct(nb, gb);
    claims.push(Claim {
        name: "Graph cuts navigation cost".into(),
        threshold:
            "all correct, reachability round-trips graph<naive, reachability bytes saved >= 50%"
                .into(),
        measured: format!(
            "correct {correct}/{}, rt-win {rt_win}, reachability saved {reach_saved}%",
            nav.len()
        ),
        pass: correct == nav.len() && rt_win && reach_saved >= 50,
    });

    Ok(claims)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let claims = compute_claims().await?;
    println!("# lens claim proof (deterministic, no model)\n");
    println!("| Claim | Threshold | Measured | Verdict |");
    println!("| --- | --- | --- | :---: |");
    for c in &claims {
        println!(
            "| {} | {} | {} | {} |",
            c.name,
            c.threshold,
            c.measured,
            if c.pass { "PASS" } else { "FAIL" }
        );
    }
    let failed = claims.iter().filter(|c| !c.pass).count();
    println!(
        "\n{} / {} claims pass.",
        claims.len() - failed,
        claims.len()
    );
    if failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_deterministic_claim_passes() {
        let claims = compute_claims().await.expect("compute_claims failed");
        for c in &claims {
            assert!(c.pass, "claim failed: {} (measured {})", c.name, c.measured);
        }
    }
}
