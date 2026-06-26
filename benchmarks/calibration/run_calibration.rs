//! bench_calibration — proves the generic tags.scm adapter tracks the hand-written
//! extraction on the two languages where lens holds BOTH a hand-written oracle and a
//! real corpus: Rust (lens's own `src/`) and Python (a committed fixture).
//!
//! For each oracle language it runs the hand-written spec and the tags adapter over
//! the same files, then measures how well the adapter recovers the oracle's symbols
//! and calls. Defs/calls are keyed by `(file, name)`, so a coarser KIND label (Rust
//! `struct`/`enum` -> tags `class`) is NOT counted as a miss; only a genuinely
//! missed symbol is. The (explained) delta — tags.scm omits Rust `const` and
//! trait-method signatures and all imports, and adds unions/macros — is frozen as a
//! golden baseline.
//!
//! Gates are drift-tolerant ratio floors (`cur + EPS >= base`) so the live `src/`
//! corpus can grow without spurious failure; only a real adapter regression trips
//! them. Offline + deterministic: captured twice per run as an explicit determinism
//! check (the plan's "run twice -> identical" predicate).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use lens::discovery::extract::{extract_file, spec_for_language};
use lens::discovery::tags_adapter::{extract_tags_file, oracle_tags_specs, TagsLangSpec};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn calibration_path() -> PathBuf {
    repo_root().join("benchmarks/calibration/expected/calibration.json")
}

/// The corpus directory for an oracle language.
fn corpus_for(lang: &str) -> PathBuf {
    match lang {
        "rust" => repo_root().join("src"),
        "python" => repo_root().join("benchmarks/calibration/fixtures/python"),
        other => panic!("no calibration corpus mapped for {other}"),
    }
}

/// One language's adapter-vs-oracle comparison. Counts are informational (they
/// drift with the corpus); the ratio fields are the gated, drift-tolerant proof.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LangCalibration {
    language: String,
    files: usize,
    hand_defs: usize,
    tags_defs: usize,
    shared_defs: usize,
    /// Fraction of oracle def symbols the adapter also finds (the headline proof).
    def_recall: f64,
    /// Fraction of adapter def symbols that are real oracle symbols.
    def_precision: f64,
    hand_calls: usize,
    tags_calls: usize,
    shared_calls: usize,
    call_recall: f64,
    hand_imports: usize,
    tags_imports: usize,
    /// Up to 12 symbol names the oracle finds but the adapter misses (explained delta).
    hand_only_sample: Vec<String>,
    /// Up to 12 symbol names the adapter finds but the oracle does not.
    tags_only_sample: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Calibration {
    langs: Vec<LangCalibration>,
}

fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn ratio(num: usize, den: usize) -> f64 {
    if den == 0 {
        1.0
    } else {
        round4(num as f64 / den as f64)
    }
}

fn collect_lang(spec: &TagsLangSpec) -> LangCalibration {
    let hand = spec_for_language(spec.name).expect("oracle language has a hand-written spec");
    let corpus = corpus_for(spec.name);
    let ext = spec.extensions[0];

    let mut hand_defs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut tags_defs: BTreeSet<(String, String)> = BTreeSet::new();
    let mut hand_calls: BTreeSet<(String, String)> = BTreeSet::new();
    let mut tags_calls: BTreeSet<(String, String)> = BTreeSet::new();
    let (mut hand_imports, mut tags_imports, mut files) = (0usize, 0usize, 0usize);

    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(&corpus)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some(ext))
        .collect();
    paths.sort();

    for path in &paths {
        let rel = path
            .strip_prefix(&corpus)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(fx) = extract_file(&rel, &content, &hand) {
            files += 1;
            for d in &fx.defs {
                hand_defs.insert((rel.clone(), d.name.clone()));
            }
            for (_, callee) in &fx.calls {
                hand_calls.insert((rel.clone(), callee.clone()));
            }
            hand_imports += fx.imports.len();
        }
        if let Some(fx) = extract_tags_file(&rel, &content, spec) {
            for d in &fx.defs {
                tags_defs.insert((rel.clone(), d.name.clone()));
            }
            for (_, callee) in &fx.calls {
                tags_calls.insert((rel.clone(), callee.clone()));
            }
            tags_imports += fx.imports.len();
        }
    }

    let shared_defs = hand_defs.intersection(&tags_defs).count();
    let shared_calls = hand_calls.intersection(&tags_calls).count();

    let sample = |a: &BTreeSet<(String, String)>, b: &BTreeSet<(String, String)>| {
        let mut names: Vec<String> = a.difference(b).map(|(_, n)| n.clone()).collect();
        names.sort();
        names.dedup();
        names.truncate(12);
        names
    };

    LangCalibration {
        language: spec.name.to_string(),
        files,
        hand_defs: hand_defs.len(),
        tags_defs: tags_defs.len(),
        shared_defs,
        def_recall: ratio(shared_defs, hand_defs.len()),
        def_precision: ratio(shared_defs, tags_defs.len()),
        hand_calls: hand_calls.len(),
        tags_calls: tags_calls.len(),
        shared_calls,
        call_recall: ratio(shared_calls, hand_calls.len()),
        hand_imports,
        tags_imports,
        hand_only_sample: sample(&hand_defs, &tags_defs),
        tags_only_sample: sample(&tags_defs, &hand_defs),
    }
}

fn capture() -> Calibration {
    let mut langs: Vec<LangCalibration> = oracle_tags_specs().iter().map(collect_lang).collect();
    langs.sort_by(|a, b| a.language.cmp(&b.language));
    Calibration { langs }
}

fn load_baseline() -> Calibration {
    std::fs::read_to_string(calibration_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn main() -> anyhow::Result<()> {
    let cur = capture();
    // Determinism gate: a second capture over the same corpus must be identical.
    if cur != capture() {
        eprintln!("bench_calibration: NON-DETERMINISTIC (two captures of the same corpus differ)");
        std::process::exit(1);
    }

    if std::env::args().any(|a| a == "--update") {
        let path = calibration_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&cur)? + "\n")?;
        eprintln!("captured calibration baseline: {}", path.display());
        println!("{}", serde_json::to_string_pretty(&cur)?);
        return Ok(());
    }

    let base = load_baseline();
    if base.langs.is_empty() {
        eprintln!(
            "no calibration baseline at {} — run `cargo run --release --bin bench_calibration -- --update` first",
            calibration_path().display()
        );
        std::process::exit(1);
    }

    // Ratio floors tolerate live-src/ drift; only a real regression trips them.
    const EPS: f64 = 0.02;
    let mut fail = false;
    let mut out = String::from("# bench_calibration (tags adapter vs hand-written oracle)\n\n");
    out.push_str("| language | files | def recall | def precision | call recall | imports hand->tags | verdict |\n");
    out.push_str("|---|---:|---:|---:|---:|---|---|\n");

    for bl in &base.langs {
        match cur.langs.iter().find(|l| l.language == bl.language) {
            None => {
                fail = true;
                out.push_str(&format!("| {} | (vanished) |  |  |  |  | FAIL |\n", bl.language));
            }
            Some(c) => {
                let ok = c.def_recall + EPS >= bl.def_recall
                    && c.def_precision + EPS >= bl.def_precision
                    && c.call_recall + EPS >= bl.call_recall;
                fail |= !ok;
                out.push_str(&format!(
                    "| {} | {} | {:.3} (base {:.3}) | {:.3} (base {:.3}) | {:.3} (base {:.3}) | {}->{} | {} |\n",
                    c.language,
                    c.files,
                    c.def_recall,
                    bl.def_recall,
                    c.def_precision,
                    bl.def_precision,
                    c.call_recall,
                    bl.call_recall,
                    c.hand_imports,
                    c.tags_imports,
                    if ok { "PASS" } else { "FAIL" },
                ));
            }
        }
    }

    println!("{out}");
    // Echo the explained delta so the proof is legible, not just a number.
    for c in &cur.langs {
        println!(
            "[{}] {} oracle defs, {} adapter defs, {} shared. oracle-only e.g. {:?}; adapter-only e.g. {:?}",
            c.language, c.hand_defs, c.tags_defs, c.shared_defs, c.hand_only_sample, c.tags_only_sample
        );
    }

    if fail {
        eprintln!("bench_calibration: FAIL (adapter recall/precision regressed below the frozen floor)");
        std::process::exit(1);
    }
    println!("bench_calibration: PASS");
    Ok(())
}
