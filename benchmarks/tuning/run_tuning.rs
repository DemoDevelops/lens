//! bench_tuning - cold-build A/B: SQLite FTS5 (baseline) vs Tantivy (current backend).
//!
//! `cargo run --release --bin bench_tuning`                 (synthetic scaling curve)
//! `LENS_BENCH_REAL=<dir> cargo run --release --bin bench_tuning`  (real-corpus curve)
//!
//! WHY THIS EXISTS: the pre-Tantivy measurement (branch `perf/index-sqlite-tuning`)
//! found the FTS5 index build is ~O(n^2) in chunks and, being single-writer, cannot use
//! more than one core; PRAGMA tuning was refuted (3-13% slower at real scale). Tantivy
//! builds independent segments across N threads and merges them, which is the one
//! architectural way to use every core for indexing. This harness times a cold full
//! build of the SAME corpus at increasing scales under BOTH backends on one machine and
//! reports the curve + speedup, so the win is a within-run A/B, never an absolute ms.
//!
//! WHAT IS REAL vs REPLICATED:
//!   * tantivy arm  -> the REAL `Index::open` + `Index::index_path` (production build).
//!   * sqlite arm   -> a faithful replica of the REMOVED FTS5 build path (the two
//!                     `chunks`/`chunks_tri` vtables, 100-line content windows, a single
//!                     bulk insert transaction, then `optimize`), kept ONLY as the
//!                     baseline; production no longer contains it.
//!
//! DISCLOSURES:
//!   * The sqlite replica omits the `symbols` column extraction (an empty string is
//!     stored). The real Tantivy build DOES compute symbols, so if anything this makes
//!     the sqlite arm look FASTER than a fully faithful build; the A/B is conservative
//!     against Tantivy, not for it.
//!   * The SYNTHETIC corpus replicates near-duplicate fixtures, so its term dictionary
//!     barely grows and it does NOT reach either backend's real regime (it mainly shows
//!     Tantivy's fixed per-build overhead and the crossover). For the build-time claim
//!     point `LENS_BENCH_REAL` at a large diverse tree; that curve is the real evidence.
//!   * Removed vs the prior SQLite-tuning harness (all specific to tuning SQLite, which
//!     is no longer the backend): per-stage attribution, the PRAGMA read/write config
//!     matrices, the connection-reuse probe, and the byte-identical-across-configs
//!     correctness gate. Ranking parity is now gated by the inline tests in
//!     src/index/mod.rs and tests/index_tests.rs.

#![allow(clippy::doc_overindented_list_items, clippy::doc_lazy_continuation)]

use std::collections::BTreeSet;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json::{json, Value};

use lens::index::{file_manifest, Index};
use lens::obs::configure_conn;

/// Synthetic corpus multipliers (near-duplicate; see the module disclosures).
const SCALES: &[usize] = &[1, 10, 50, 100, 200, 400];
/// Real-corpus subset sizes in files (capped at what the tree actually has).
const REAL_SUBSETS: &[usize] = &[500, 1000, 2000, 4000, 8000, 16000];
/// Ceiling on files pulled from a real tree.
const REAL_CAP: usize = 16000;
/// Lines per code chunk (mirrors `CODE_WINDOW` in src/index/mod.rs).
const CODE_WINDOW: usize = 100;
/// Repetitions per (scale, backend); the median is reported to damp scheduler noise.
const REPS: usize = 3;

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/savings/workloads/code_search")
}

/// Deterministic corpus scaler, mirroring `benchmarks/common/savings.rs::replicate_tree`
/// (1x = byte-identical; k>0 prepends a unique comment so trigram/BM25 still match).
fn scale_corpus(src: &Path, dst: &Path, scale: usize) {
    for entry in walkdir::WalkDir::new(src).into_iter().flatten() {
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(src).unwrap();
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let stem = rel.file_stem().and_then(|s| s.to_str()).unwrap_or("f");
        let ext = rel.extension().and_then(|s| s.to_str()).unwrap_or("");
        for k in 0..scale {
            let name = if k == 0 {
                rel.file_name().unwrap().to_string_lossy().to_string()
            } else if ext.is_empty() {
                format!("{stem}_c{k}")
            } else {
                format!("{stem}_c{k}.{ext}")
            };
            std::fs::create_dir_all(dst).unwrap();
            let body = if k == 0 {
                content.clone()
            } else {
                format!("// copy {k}\n{content}")
            };
            std::fs::write(dst.join(name), body).unwrap();
        }
    }
}

/// Collect up to `cap` code/text files under `root` (a real, diverse corpus source),
/// in a stable walk order so subsets are prefixes of one another.
fn collect_files(root: &Path, cap: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in walkdir::WalkDir::new(root)
        .sort_by_file_name()
        .into_iter()
        .flatten()
    {
        if out.len() >= cap {
            break;
        }
        if !e.file_type().is_file() {
            continue;
        }
        let ok = e
            .path()
            .extension()
            .and_then(|x| x.to_str())
            .map(|x| {
                matches!(
                    x,
                    "rs" | "py" | "js" | "ts" | "go" | "c" | "h" | "cpp" | "java" | "rb" | "toml"
                )
            })
            .unwrap_or(false);
        if ok {
            out.push(e.path().to_path_buf());
        }
    }
    out
}

/// Copy the first `n` of `files` into a fresh temp dir under flat unique names. Copy cost
/// is outside the timed build region (measured on the returned dir afterwards).
fn subset_dir(files: &[PathBuf], n: usize) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (i, f) in files.iter().take(n).enumerate() {
        let ext = f.extension().and_then(|x| x.to_str()).unwrap_or("txt");
        if let Ok(bytes) = std::fs::read(f) {
            let _ = std::fs::write(dir.path().join(format!("f{i}.{ext}")), bytes);
        }
    }
    dir
}

/// 100-line windows of `content`, dropping empties; the same chunking the code path of
/// `chunk_file` produces.
fn chunks_of(content: &str) -> Vec<String> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }
    lines
        .chunks(CODE_WINDOW)
        .map(|w| w.join("\n"))
        .filter(|c| !c.trim().is_empty())
        .collect()
}

/// Cold full build under the REMOVED SQLite FTS5 path (baseline). Returns (ms, rows).
fn sqlite_cold_build(corpus: &Path) -> (f64, i64) {
    let data = tempfile::tempdir().unwrap();
    let db = data.path().join("index.db");
    let t = Instant::now();
    let mut conn = Connection::open(&db).unwrap();
    configure_conn(&conn).unwrap(); // the REAL production WAL + busy_handler config
    conn.execute_batch(
        "CREATE VIRTUAL TABLE chunks USING fts5(
             path UNINDEXED, chunk_id UNINDEXED, symbols, content, tokenize = 'porter');
         CREATE VIRTUAL TABLE chunks_tri USING fts5(
             path UNINDEXED, chunk_id UNINDEXED, content, tokenize = 'trigram');",
    )
    .unwrap();
    {
        let tx = conn.transaction().unwrap();
        {
            let mut ins = tx
                .prepare(
                    "INSERT INTO chunks (path, chunk_id, symbols, content) VALUES (?1, ?2, '', ?3)",
                )
                .unwrap();
            let mut ins_tri = tx
                .prepare("INSERT INTO chunks_tri (path, chunk_id, content) VALUES (?1, ?2, ?3)")
                .unwrap();
            for entry in walkdir::WalkDir::new(corpus).into_iter().flatten() {
                if !entry.file_type().is_file() {
                    continue;
                }
                let content = match std::fs::read_to_string(entry.path()) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let path = entry.path().to_string_lossy();
                for (i, chunk) in chunks_of(&content).iter().enumerate() {
                    let chunk_id = format!("{path}#{i}");
                    ins.execute(params![path, chunk_id, chunk]).unwrap();
                    ins_tri.execute(params![path, chunk_id, chunk]).unwrap();
                }
            }
        }
        tx.commit().unwrap();
    }
    // Collapse the segment forest a bulk build leaves (the old index_path did this).
    conn.execute_batch(
        "INSERT INTO chunks(chunks) VALUES('optimize');
         INSERT INTO chunks_tri(chunks_tri) VALUES('optimize');",
    )
    .unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let rows: i64 = conn
        .query_row("SELECT count(*) FROM chunks", [], |r| r.get(0))
        .unwrap();
    (ms, rows)
}

/// Cold full build under the production Tantivy path. Returns (ms, docs).
fn tantivy_cold_build(corpus: &Path) -> (f64, i64) {
    let data = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let idx = Index::open(data.path()).unwrap();
    idx.index_path(corpus, true).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let docs = idx.chunk_count().unwrap();
    black_box(&idx);
    (ms, docs)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if v.is_empty() {
        0.0
    } else {
        v[v.len() / 2]
    }
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Median build ms for each backend at one corpus, interleaving reps to avoid a warm-cache
/// bias toward whichever arm runs second. Returns (sqlite_ms, rows, tantivy_ms, docs).
fn measure(corpus: &Path) -> (f64, i64, f64, i64) {
    let (mut sq, mut tv) = (Vec::new(), Vec::new());
    let (mut sq_rows, mut tv_docs) = (0i64, 0i64);
    for _ in 0..REPS {
        let (s, sr) = sqlite_cold_build(corpus);
        let (t, td) = tantivy_cold_build(corpus);
        sq.push(s);
        tv.push(t);
        sq_rows = sr;
        tv_docs = td;
    }
    (median(sq), sq_rows, median(tv), tv_docs)
}

/// Measure one point and emit its table row + JSON.
fn report_point(label: &str, nfiles: usize, corpus: &Path) -> Value {
    let (sq_ms, sq_rows, tv_ms, tv_docs) = measure(corpus);
    let speedup = if tv_ms > 0.0 { sq_ms / tv_ms } else { 0.0 };
    let sq_per = if sq_rows > 0 {
        sq_ms * 1000.0 / sq_rows as f64
    } else {
        0.0
    };
    let tv_per = if tv_docs > 0 {
        tv_ms * 1000.0 / tv_docs as f64
    } else {
        0.0
    };
    println!(
        "| {label} | {nfiles} | {tv_docs} | {} | {} | {:.2}x | {:.1} | {:.1} |",
        round1(sq_ms),
        round1(tv_ms),
        speedup,
        sq_per,
        tv_per
    );
    json!({
        "label": label, "files": nfiles, "chunks": tv_docs,
        "sqlite_ms": round1(sq_ms), "tantivy_ms": round1(tv_ms),
        "speedup": round1(speedup),
        "sqlite_us_per_chunk": round1(sq_per), "tantivy_us_per_chunk": round1(tv_per),
        "sqlite_rows": sq_rows,
    })
}

fn machine_info() -> Value {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    json!({
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "logical_cpus": cpus,
        "unix_time": ts,
        "reps": REPS,
    })
}

fn main() {
    let mi = machine_info();
    let real = std::env::var("LENS_BENCH_REAL").ok();
    println!("# bench_tuning - cold-build A/B: SQLite FTS5 vs Tantivy\n");
    println!(
        "machine: {} / {} / {} logical CPUs   (reps={REPS}, median reported)",
        mi["os"], mi["arch"], mi["logical_cpus"]
    );
    println!(
        "mode: {}\n",
        match &real {
            Some(p) => format!("REAL corpus subsets from {p}"),
            None => "SYNTHETIC scaling (near-duplicate; shows overhead + crossover only)".into(),
        }
    );

    println!("| point | files | chunks | sqlite ms | tantivy ms | speedup | sqlite us/chunk | tantivy us/chunk |");
    println!("|------:|------:|-------:|----------:|-----------:|--------:|----------------:|-----------------:|");

    let points: Vec<Value> = if let Some(realroot) = &real {
        let files = collect_files(&PathBuf::from(realroot), REAL_CAP);
        let sizes: Vec<usize> = REAL_SUBSETS
            .iter()
            .cloned()
            .chain(std::iter::once(files.len()))
            .filter(|&n| n > 0 && n <= files.len())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        sizes
            .iter()
            .map(|&n| {
                let dir = subset_dir(&files, n);
                report_point(&n.to_string(), n, dir.path())
            })
            .collect()
    } else {
        SCALES
            .iter()
            .map(|&scale| {
                let corpus = tempfile::tempdir().unwrap();
                scale_corpus(&corpus_root(), corpus.path(), scale);
                let nfiles = file_manifest(corpus.path()).len();
                report_point(&format!("{scale}x"), nfiles, corpus.path())
            })
            .collect()
    };

    // Honest summary: the crossover point (first where tantivy build < sqlite build), the
    // max speedup, and whether sqlite's per-chunk cost RISES across the larger half of the
    // curve (the super-linear signature) while tantivy's stays flat (amortized overhead).
    let f = |p: &Value, k: &str| p[k].as_f64().unwrap_or(0.0);
    let crossover = points
        .iter()
        .find(|p| f(p, "tantivy_ms") < f(p, "sqlite_ms"))
        .map(|p| p["chunks"].as_i64().unwrap_or(0));
    let max_speedup = points
        .iter()
        .map(|p| f(p, "speedup"))
        .fold(0.0_f64, f64::max);
    let half = points.len() / 2;
    let per_chunk_trend = |arm: &str| -> f64 {
        let mid = points.get(half).map(|p| f(p, arm)).unwrap_or(0.0);
        let last = points.last().map(|p| f(p, arm)).unwrap_or(0.0);
        if mid > 0.0 {
            last / mid
        } else {
            0.0
        }
    };
    let sq_trend = per_chunk_trend("sqlite_us_per_chunk");
    let tv_trend = per_chunk_trend("tantivy_us_per_chunk");
    println!(
        "\ncrossover: {}   max speedup: {max_speedup:.2}x",
        match crossover {
            Some(c) => format!("tantivy overtakes sqlite at ~{c} chunks"),
            None => "tantivy never overtakes sqlite at the tested sizes".into(),
        }
    );
    println!(
        "per-chunk cost over the larger half: sqlite x{sq_trend:.2} ({}), tantivy x{tv_trend:.2} ({})",
        if sq_trend > 1.15 {
            "RISING = super-linear"
        } else {
            "~flat = linear"
        },
        if tv_trend <= 1.15 {
            "~flat = linear"
        } else {
            "rising"
        }
    );

    let out = json!({
        "machine": mi,
        "mode": if real.is_some() { "real" } else { "synthetic" },
        "real_root": real,
        "points": points,
        "crossover_chunks": crossover,
        "max_speedup": round1(max_speedup),
        "sqlite_per_chunk_trend": round1(sq_trend),
        "tantivy_per_chunk_trend": round1(tv_trend),
    });
    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/tuning/results");
    std::fs::create_dir_all(&results_dir).unwrap();
    let name = if real.is_some() {
        "ab_build_real.json"
    } else {
        "ab_build.json"
    };
    let outfile = results_dir.join(name);
    std::fs::write(&outfile, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    println!("\nwrote {}", outfile.display());
}
