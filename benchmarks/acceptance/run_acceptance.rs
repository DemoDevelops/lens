//! bench_acceptance — a FLOOR (not proof) for languages with no hand-written
//! oracle. Per language it runs extraction over a high-star OSS repo pinned to a
//! commit SHA, and gates two automatable signals:
//!   * parse-success rate  — catches grammar-version breakage (files that no
//!     longer parse against lens's tree-sitter).
//!   * coverage sanity     — hundreds of functions must yield hundreds of def
//!     nodes, not 3 (catches a silently mis-wired adapter).
//!
//! The corpus is large and external, so it is NOT committed: `fetch_corpus.sh`
//! shallow-clones each pinned SHA into the gitignored `corpus/<lang>/`. This
//! harness gates whatever corpus is PRESENT and SKIPs the rest, so it is green on a
//! fresh checkout (nothing to check) and meaningful once the corpus is cached. SHAs
//! are pinned (never floating `main`) so the gate is reproducible.
//!
//! Cross-checking against the grammar's own `tree-sitter tags` would be mostly
//! circular (same TAGS_QUERY), so this validates adapter glue + grammar health,
//! not extraction semantics. That is the calibration harness's job.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use lens::discovery::tags_adapter::any_spec_for_extension;

fn acceptance_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/acceptance")
}

/// One language's pinned corpus + gate floors (from thresholds.json).
#[derive(Debug, Deserialize)]
struct Threshold {
    /// Source repo (documentation + used by fetch_corpus.sh).
    #[allow(dead_code)]
    repo: String,
    /// Pinned commit SHA (used by fetch_corpus.sh; never a floating branch).
    #[allow(dead_code)]
    sha: String,
    /// Minimum fraction of candidate files that must parse.
    min_parse_rate: f64,
    /// Minimum total def nodes (coverage floor; proves real extraction).
    min_defs: usize,
}

#[derive(Debug)]
struct LangResult {
    lang: String,
    status: Status,
    files: usize,
    parsed: usize,
    parse_rate: f64,
    defs: usize,
    calls: usize,
}

#[derive(Debug, PartialEq)]
enum Status {
    Pass,
    Fail(String),
    Skip,
}

/// Walk `corpus/<lang>/`, extracting every file that dispatches to `lang`, and tally
/// parse-success + def/call coverage. Returns `None` if the corpus dir is absent.
fn measure(lang: &str, corpus: &PathBuf) -> Option<(usize, usize, usize, usize)> {
    if !corpus.exists() {
        return None;
    }
    let (mut files, mut parsed, mut defs, mut calls) = (0usize, 0usize, 0usize, 0usize);
    for entry in walkdir::WalkDir::new(corpus).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|s| s.to_str()) {
            Some(e) => e,
            None => continue,
        };
        // Only files this language actually owns (the corpus is single-language, but
        // repos carry stray configs/docs we must not count against the parse rate).
        let spec = match any_spec_for_extension(ext) {
            Some(s) if s.name() == lang => s,
            _ => continue,
        };
        files += 1;
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // unreadable/non-utf8: counts against parse rate
        };
        let rel = path.strip_prefix(corpus).unwrap_or(path).to_string_lossy();
        if let Some(fx) = spec.extract_file(&rel, &content) {
            parsed += 1;
            defs += fx.defs.len();
            calls += fx.calls.len();
        }
    }
    Some((files, parsed, defs, calls))
}

fn main() -> anyhow::Result<()> {
    let thresholds_path = acceptance_dir().join("thresholds.json");
    let raw = std::fs::read_to_string(&thresholds_path)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", thresholds_path.display()))?;
    let thresholds: BTreeMap<String, Threshold> = serde_json::from_str(&raw)?;

    let mut results: Vec<LangResult> = Vec::new();
    for (lang, t) in &thresholds {
        let corpus = acceptance_dir().join("corpus").join(lang);
        match measure(lang, &corpus) {
            None => results.push(LangResult {
                lang: lang.clone(),
                status: Status::Skip,
                files: 0,
                parsed: 0,
                parse_rate: 0.0,
                defs: 0,
                calls: 0,
            }),
            Some((files, parsed, defs, calls)) => {
                let rate = if files > 0 {
                    parsed as f64 / files as f64
                } else {
                    0.0
                };
                let status = if files == 0 {
                    Status::Fail("corpus present but no files of this language".into())
                } else if rate < t.min_parse_rate {
                    Status::Fail(format!("parse rate {:.3} < {:.3}", rate, t.min_parse_rate))
                } else if defs < t.min_defs {
                    Status::Fail(format!("defs {} < floor {}", defs, t.min_defs))
                } else {
                    Status::Pass
                };
                results.push(LangResult {
                    lang: lang.clone(),
                    status,
                    files,
                    parsed,
                    parse_rate: rate,
                    defs,
                    calls,
                });
            }
        }
    }

    let mut out = String::from("# bench_acceptance (pinned real-world corpus floor)\n\n");
    out.push_str("| language | files | parsed | parse rate | defs | calls | verdict |\n");
    out.push_str("|---|---:|---:|---:|---:|---:|---|\n");
    let (mut checked, mut skipped, mut fail) = (0usize, 0usize, false);
    for r in &results {
        let verdict = match &r.status {
            Status::Pass => {
                checked += 1;
                "PASS".to_string()
            }
            Status::Skip => {
                skipped += 1;
                "SKIP (no corpus)".to_string()
            }
            Status::Fail(why) => {
                checked += 1;
                fail = true;
                format!("FAIL: {why}")
            }
        };
        out.push_str(&format!(
            "| {} | {} | {} | {:.3} | {} | {} | {} |\n",
            r.lang, r.files, r.parsed, r.parse_rate, r.defs, r.calls, verdict
        ));
    }
    println!("{out}");
    println!("{checked} language(s) gated, {skipped} skipped (corpus not fetched; run benchmarks/acceptance/fetch_corpus.sh)");

    if fail {
        eprintln!("bench_acceptance: FAIL (a pinned corpus regressed parse-rate or coverage)");
        std::process::exit(1);
    }
    println!("bench_acceptance: PASS");
    Ok(())
}
