//! bench_fitness — held-out, human-owned validation gate for the lens
//! improvement loop.
//!
//! Division of labor:
//!   * `bench_changes` proves each SPECIFIC fix (C5-C15), authored alongside
//!     the fix it gates.
//!   * `bench_fitness` (this) is the GLOBAL ratchet. It proves a change did not
//!     regress real end-user value (o200k token savings on fixed, realistic
//!     corpora) and did not erode the lens ethos (no new runtime dependency, no
//!     silent change to the public MCP tool surface).
//!
//! The implement-agent in the loop must NEVER edit this file, the corpora it
//! measures, or `fitness/expected/baseline.json`. A human gate-keeper owns
//! those. Run `--update` ONLY by hand, to accept a new baseline (e.g. after
//! intentionally adding a tool or a dependency).
//!
//! Offline + deterministic: same committed fixtures + same code => same numbers.
//! No model call, no network, no API key.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[path = "../common/savings.rs"]
#[allow(dead_code)]
mod savings;
use savings::{bench_root, compute_savings, SavingsRow};

/// Per-workload token savings, for the regression tripwire.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkloadSavings {
    workload: String,
    token_savings_pct: f64,
}

/// The frozen fitness snapshot. `--update` writes it; gates compare against it.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Fitness {
    /// Overall o200k token savings across all workloads, percent.
    overall_token_savings_pct: f64,
    /// Per-workload token savings, percent.
    per_workload: Vec<WorkloadSavings>,
    /// Number of entries in the root Cargo.toml `[dependencies]` table.
    dep_count: usize,
    /// Public MCP tool names (the `lens_*` tools in src/server.rs), sorted.
    tools: Vec<String>,
}

fn fitness_path() -> PathBuf {
    bench_root().join("fitness/expected/baseline.json")
}

fn repo_root() -> PathBuf {
    bench_root()
        .parent()
        .expect("benchmarks/ has a parent")
        .to_path_buf()
}

/// Count entries in the `[dependencies]` table of the root Cargo.toml.
fn dep_count() -> anyhow::Result<usize> {
    let toml = std::fs::read_to_string(repo_root().join("Cargo.toml"))?;
    let mut in_deps = false;
    let mut n = 0;
    for line in toml.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_deps = t == "[dependencies]";
            continue;
        }
        if in_deps && !t.is_empty() && !t.starts_with('#') && t.contains('=') {
            n += 1;
        }
    }
    Ok(n)
}

/// Public MCP tool names, scraped from the `async fn lens_*` tool methods in
/// src/server.rs. Reads the source (robust against internal-visibility changes)
/// and relies on the project's `lens_*` tool-naming convention.
fn tool_names() -> anyhow::Result<Vec<String>> {
    let src = std::fs::read_to_string(repo_root().join("src/server.rs"))?;
    let mut names = BTreeSet::new();
    for line in src.lines() {
        if let Some(idx) = line.find("async fn lens_") {
            let rest = &line[idx + "async fn ".len()..];
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            names.insert(name);
        }
    }
    Ok(names.into_iter().collect())
}

/// Aggregate token savings overall and per workload from the savings rows. All
/// arms are now deterministic across checkouts (FTS search tie-breaks are stable,
/// see src/index/mod.rs `ORDER BY ..., path, chunk_id`), so all are gated.
fn savings_from_rows(rows: &[SavingsRow]) -> (f64, Vec<WorkloadSavings>) {
    let pct = |b: usize, a: usize| -> f64 {
        if b > 0 {
            (b.saturating_sub(a) as f64 / b as f64) * 100.0
        } else {
            0.0
        }
    };
    let (mut tot_b, mut tot_a) = (0usize, 0usize);
    let mut by: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in rows {
        tot_b += r.before_tokens;
        tot_a += r.after_tokens;
        let e = by.entry(r.workload.clone()).or_default();
        e.0 += r.before_tokens;
        e.1 += r.after_tokens;
    }
    let per = by
        .into_iter()
        .map(|(workload, (b, a))| WorkloadSavings {
            workload,
            token_savings_pct: pct(b, a),
        })
        .collect();
    (pct(tot_b, tot_a), per)
}

async fn capture() -> anyhow::Result<Fitness> {
    let rows = compute_savings().await?;
    let (overall, per_workload) = savings_from_rows(&rows);
    Ok(Fitness {
        overall_token_savings_pct: overall,
        per_workload,
        dep_count: dep_count()?,
        tools: tool_names()?,
    })
}

fn load_baseline() -> Fitness {
    std::fs::read_to_string(fitness_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cur = capture().await?;

    if std::env::args().any(|a| a == "--update") {
        let path = fitness_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&cur)? + "\n")?;
        eprintln!("captured fitness baseline: {}", path.display());
        println!("{}", serde_json::to_string_pretty(&cur)?);
        return Ok(());
    }

    let base = load_baseline();
    if base.tools.is_empty() && base.dep_count == 0 {
        eprintln!(
            "no fitness baseline at {} — run `cargo run --bin bench_fitness -- --update` first",
            fitness_path().display()
        );
        std::process::exit(1);
    }

    let mut fail = false;
    let mut out = String::from("# bench_fitness (held-out value + invariant ratchet)\n\n");
    out.push_str("| gate | baseline | current | verdict |\n|---|---|---|---|\n");

    // G1: overall token savings must not regress.
    const EPS: f64 = 0.05;
    let g1 = cur.overall_token_savings_pct + EPS >= base.overall_token_savings_pct;
    fail |= !g1;
    out.push_str(&format!(
        "| overall token savings | {:.1}% | {:.1}% | {} |\n",
        base.overall_token_savings_pct,
        cur.overall_token_savings_pct,
        verdict(g1)
    ));

    // G1b: per-workload tripwire (1.0pp tolerance); a vanished workload fails.
    const TOL: f64 = 1.0;
    for bw in &base.per_workload {
        let cw = cur.per_workload.iter().find(|w| w.workload == bw.workload);
        let (ok, cur_pct) = match cw {
            Some(w) => (w.token_savings_pct + TOL >= bw.token_savings_pct, w.token_savings_pct),
            None => (false, f64::NAN),
        };
        fail |= !ok;
        out.push_str(&format!(
            "| savings: {} | {:.1}% | {:.1}% | {} |\n",
            bw.workload,
            bw.token_savings_pct,
            cur_pct,
            verdict(ok)
        ));
    }

    // G2: no new runtime dependency (bloat guard).
    let g2 = cur.dep_count <= base.dep_count;
    fail |= !g2;
    out.push_str(&format!(
        "| dependency count | {} | {} | {} |\n",
        base.dep_count,
        cur.dep_count,
        verdict(g2)
    ));

    // G3: public MCP tool surface unchanged.
    let bset: BTreeSet<&String> = base.tools.iter().collect();
    let cset: BTreeSet<&String> = cur.tools.iter().collect();
    let g3 = bset == cset;
    fail |= !g3;
    let detail = if g3 {
        format!("{} tools", cur.tools.len())
    } else {
        let added: Vec<&str> = cset.difference(&bset).map(|s| s.as_str()).collect();
        let removed: Vec<&str> = bset.difference(&cset).map(|s| s.as_str()).collect();
        format!("added {added:?} removed {removed:?}")
    };
    out.push_str(&format!(
        "| tool surface | {} tools | {} | {} |\n",
        base.tools.len(),
        detail,
        verdict(g3)
    ));

    println!("{out}");
    if fail {
        eprintln!("bench_fitness: FAIL (a gate regressed or an invariant broke; if intentional, a human re-runs --update)");
        std::process::exit(1);
    }
    println!("bench_fitness: PASS");
    Ok(())
}

fn verdict(ok: bool) -> &'static str {
    if ok {
        "PASS"
    } else {
        "FAIL"
    }
}
