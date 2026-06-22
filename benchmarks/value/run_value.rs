//! Plane A: per-feature value across codebase scale (deterministic, no model).
//!
//! Savings are not one number — they move with size, and sometimes the conclusion
//! flips (search ties grep on a small repo, wins 90%+ on a big one). So every
//! byte-saving dimension is measured at Small / Medium / Large / Huge and tagged with
//! how scale changes it: flat (size-insensitive), grows, or flips.
//!
//! Graph navigation is reported on ROUND-TRIPS, not bytes: its byte savings at scale
//! can't be measured honestly on the duplicate-symbol test fixture (it builds an
//! O(N^2) cross-copy edge hairball a real repo never has — see savings.rs), but its
//! round-trip win scales cleanly. Recovery (survive compaction) is a survival metric,
//! not a per-op byte saver: see bench_recovery.
//!
//!   cargo run --bin bench_value

#[path = "../common/navigation.rs"]
#[allow(dead_code)]
mod navigation;
#[path = "../common/savings.rs"]
#[allow(dead_code)]
mod savings;

use navigation::compute_navigation;
use savings::{bench_root, compute_savings, issue_triage_at, search_vs_grep_at};

// Mirrors src/wrap.rs: head+tail kept above the inline threshold.
const WRAP_PREVIEW_SIDE: usize = 2048;
const WRAP_MAX_INLINE: usize = 8192;

/// Scale tiers: a multiplier on the committed fixture, named by codebase size.
const TIERS: [(&str, usize); 4] = [("Small", 1), ("Medium", 10), ("Large", 50), ("Huge", 200)];

fn pct(before: usize, after: usize) -> i64 {
    if before == 0 {
        return 0;
    }
    (((before as f64 - after as f64) / before as f64) * 100.0).round() as i64
}

fn wrap_preview(raw: usize) -> usize {
    if raw > WRAP_MAX_INLINE {
        2 * WRAP_PREVIEW_SIDE + 24
    } else {
        raw
    }
}

/// How scale changes a row.
fn scale_effect(small: i64, huge: i64) -> &'static str {
    if small < 10 && huge >= 50 {
        "flips: ties small, wins big"
    } else if (huge - small).abs() <= 5 {
        "flat (size-insensitive)"
    } else {
        "grows with size"
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let payload =
        std::fs::read_to_string(bench_root().join("savings/workloads/issue_triage/issues.json"))?;
    let log = std::fs::read_to_string(bench_root().join("savings/workloads/log_debug/app.log"))?;

    // Sandbox is scale-invariant: the matching lines and the file both grow linearly,
    // so the ratio is flat. Take the real measured number (grep with +-2 context) so it
    // agrees with bench_prove rather than re-deriving a slightly different one.
    let savings = compute_savings().await?;
    let sandbox_pct = savings
        .iter()
        .find(|r| r.mechanism == "sandbox")
        .map(|r| pct(r.before_bytes, r.after_bytes))
        .unwrap_or(0);

    // saved% for one byte-feature at a scale multiplier.
    let saved_at = |key: &str, scale: usize| -> i64 {
        match key {
            "sandbox" => sandbox_pct, // flat
            "search" => {
                let (b, a) = search_vs_grep_at(scale);
                pct(b, a)
            }
            "compression" => {
                let (b, a) = issue_triage_at(scale);
                pct(b, a)
            }
            "wrap" => {
                let raw = payload.len() * scale;
                pct(raw, wrap_preview(raw))
            }
            "redirect" => {
                let raw = log.len() * scale;
                pct(raw, "root cause: ConnectionTimeout (x12)".len())
            }
            _ => 0,
        }
    };

    let features = [
        ("Crunch a big file/log, return the answer", "sandbox"),
        (
            "Find where something is across the repo (vs grep)",
            "search",
        ),
        (
            "Shrink repetitive structured data (big JSON)",
            "compression",
        ),
        ("Run a noisy command, keep a preview", "wrap"),
        ("Stop a web page / build log flooding the chat", "redirect"),
    ];

    println!("# ctxforge value across codebase scale (Plane A: deterministic, % bytes saved)\n");
    let header: Vec<String> = TIERS.iter().map(|(n, m)| format!("{n} ({m}x)")).collect();
    println!(
        "| What it does for you | {} | scale effect |",
        header.join(" | ")
    );
    println!(
        "| --- | {} | --- |",
        TIERS.iter().map(|_| "---:").collect::<Vec<_>>().join(" | ")
    );
    for (label, key) in features {
        let saved: Vec<i64> = TIERS.iter().map(|(_, m)| saved_at(key, *m)).collect();
        let cells: Vec<String> = saved.iter().map(|s| format!("{s}%")).collect();
        let effect = scale_effect(saved[0], *saved.last().unwrap());
        println!("| {} | {} | {} |", label, cells.join(" | "), effect);
    }

    // Graph: round-trip axis (scales cleanly; bytes are confounded by the fixture).
    let nav = compute_navigation()?;
    let reach: Vec<_> = nav.iter().filter(|r| r.qtype == "path").collect();
    let nrt: usize = reach.iter().map(|r| r.naive_round_trips).sum();
    let grt: usize = reach.iter().map(|r| r.graph_round_trips).sum();
    println!("\n## Map code structure (graph): round-trip axis, scales cleanly");
    println!("| What it does for you | Small | Medium | Large | Huge | scale effect |");
    println!("| --- | ---: | ---: | ---: | ---: | --- |");
    println!(
        "| Reach/trace structure (round-trips: read each file vs 1 graph call) | {nrt}->{grt} | {}->{grt} | {}->{grt} | {}->{grt} | grows with size |",
        nrt * 10,
        nrt * 50,
        nrt * 200,
    );
    println!("\nNaive round-trips = one read per file you would trace by hand, so it grows linearly with the codebase; the graph answers in a fixed few calls regardless. (Graph BYTE savings at scale aren't shown: the duplicate-symbol test fixture inflates them into an O(N^2) artifact; the production case is bounded by Forge::maybe_compact.)");

    println!("\n## Notes");
    println!("- **flips** = the conclusion changes with size. Search is the clearest: grep is as lean on a small repo but floods on a big one — exactly when the scale-aware search nudge fires.");
    println!("- **flat** = saving is size-insensitive (crunching a file: the answer stays small whatever the file size; redirect: the raw payload never enters context at any size).");
    println!("- **grows** = the saving widens with size (wrap: the preview is bounded while raw output grows).");
    println!("- Recovery (survive a context compaction) is a survival metric, not bytes: see bench_recovery (100% vs 75% for Context Mode, ~25x fewer tokens).");
    Ok(())
}
