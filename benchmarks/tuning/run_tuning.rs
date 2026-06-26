//! bench_tuning - attribution-first benchmark for the SQLite/FTS5 tuning plan.
//!
//! `cargo run --release --bin bench_tuning`
//!
//! WHY THIS EXISTS (the honesty contract, see plan
//! `enchanted-dreaming-marble.md`): a naive before/after of `Index::search` would
//! credit "connection reuse" with a large % win that is ~0 end-to-end, because the
//! real handler runs a full gitignore-respecting repo walk (`file_manifest`) before
//! it ever opens a connection. So this harness ATTRIBUTES latency to its real
//! stages `{repo_walk, conn_open, configure_conn, prepare, execute, serialize}`,
//! measures both the isolated micro-path AND the through-handler path, runs a config
//! matrix (one PRAGMA toggled at a time, never lumped), and asserts the read-path
//! output is byte-identical across every config (this is what would catch a
//! `detail=column` "win" as the correctness regression it is, rather than a speedup).
//!
//! WHAT IS REAL vs REPLICATED:
//!   * repo_walk        -> the REAL `lens::index::file_manifest` (the exact line
//!                         every `lens_search` runs via `ensure_index`).
//!   * conn_open        -> the REAL `rusqlite::Connection::open(index.db)`.
//!   * configure_conn   -> the REAL `lens::obs::configure_conn` (WAL + busy_handler).
//!   * isolated search  -> the REAL `Index::search` (production config, conn-per-op).
//!   * config matrix    -> the VERBATIM production SQL (ranked/trigram/LIKE strings
//!                         copied from src/index/mod.rs) run on a benchmark-owned
//!                         connection that starts from the real `configure_conn` and
//!                         adds one candidate PRAGMA. The correctness gate ties this
//!                         replica to production by asserting it equals `Index::search`.
//!   * through-handler  -> the steady-state handler body via PUBLIC apis: real walk +
//!                         real `Index::search` + real `serde_json` serialize. EXCLUDES
//!                         (disclosed, all sub-microsecond): the `ops` side-channel log,
//!                         the rmcp `Json`/`Parameters` newtypes, and the tokio task
//!                         spawn. `lens_search` itself is a crate-private async fn, so a
//!                         separate bin crate cannot call it directly.
//!
//! No new dependency (bench_fitness G2). No tool-surface change (G3). Output is
//! report+JSON only; CI gating is by within-run A/B vs the A-vs-A noise floor, never
//! absolute ms.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use lens::index::{file_manifest, Index};
use lens::obs::configure_conn;
use lens::tools::SearchResponse;

// ── Tunables ────────────────────────────────────────────────────────────────
const ITERS: usize = 50; // measured iterations after warmup (plan: >=30)
const WARMUP: usize = 10;
const SCALES: &[usize] = &[1, 10, 50];
const LIMIT: usize = 5;
const W2_SCALE: usize = 10; // representative edit-loop / fragmentation scale
const FRAG_ROUNDS: usize = 64; // small incremental writes to fragment the segment forest
const CONC_PER_THREAD: usize = 60;

// Realistic query mix over the `code_search` workload.
//  - ranked (alphanumeric -> BM25F porter path): identifier lookups + a multi-term query.
const RANKED_QUERIES: &[&str] = &["Logger", "retry", "config", "connect validate", "cache"];
//  - structural (punctuation -> trigram path): `&str` (>=3 chars, trigram MATCH),
//    `->` and `::` (<3 chars, LIKE fallback).
const STRUCTURAL_QUERIES: &[&str] = &["&str", "->", "::"];

// ── Verbatim production SQL (copied from src/index/mod.rs; the correctness gate
//    ties this replica to the real `Index::search` so drift fails the build). ───
const RANKED_SQL: &str = "SELECT path, snippet(chunks, 3, '[', ']', ' … ', 24) AS snip,
                -bm25(chunks, 0.0, 0.0, 5.0, 1.0) AS score
         FROM chunks
         WHERE chunks MATCH ?1
         ORDER BY bm25(chunks, 0.0, 0.0, 5.0, 1.0), path, chunk_id
         LIMIT ?2";
const TRIGRAM_SQL: &str =
    "SELECT path, content FROM chunks_tri WHERE chunks_tri MATCH ?1 ORDER BY path, chunk_id LIMIT ?2";
const LIKE_SQL: &str =
    "SELECT path, content FROM chunks_tri WHERE content LIKE ?1 ORDER BY path, chunk_id LIMIT ?2";

// ── Candidate configs (each adds exactly one knob on top of the real baseline) ──
struct Cfg {
    name: &'static str,
    extra: &'static [&'static str],
}

// Read path: synchronous/temp_store don't touch reads, but cache_size/mmap_size do
// on a cold/large DB. At small scale these are expected to be ~0 (the OS already
// holds the whole DB); that no-op is reported, not hidden.
const READ_CONFIGS: &[Cfg] = &[
    Cfg { name: "baseline (real configure_conn: WAL)", extra: &[] },
    Cfg { name: "+cache_size=-8000 (8MB)", extra: &["PRAGMA cache_size=-8000;"] },
    Cfg { name: "+mmap_size=256MB", extra: &["PRAGMA mmap_size=268435456;"] },
    Cfg { name: "+temp_store=MEMORY", extra: &["PRAGMA temp_store=MEMORY;"] },
];

// Write path: synchronous=NORMAL is the headline candidate (fewer fsyncs per commit
// under WAL). synchronous is set EXPLICITLY here (not left to the configure_conn
// default) so the FULL-vs-NORMAL comparison stays valid after T3 makes NORMAL the
// default. temp_store=MEMORY affects large temp-btree spills.
const WRITE_CONFIGS: &[Cfg] = &[
    Cfg { name: "synchronous=FULL (pre-T3 default)", extra: &["PRAGMA synchronous=FULL;"] },
    Cfg { name: "synchronous=NORMAL (T3 default)", extra: &["PRAGMA synchronous=NORMAL;"] },
    Cfg {
        name: "synchronous=NORMAL +temp_store=MEMORY",
        extra: &["PRAGMA synchronous=NORMAL;", "PRAGMA temp_store=MEMORY;"],
    },
];

// ── Stats ────────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Stat {
    median: f64,
    p95: f64,
    p99: f64,
    min: f64,
    max: f64,
    stddev: f64,
    mean: f64,
    n: usize,
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = (p / 100.0 * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}

fn stat(mut v: Vec<f64>) -> Stat {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len().max(1);
    let mean = v.iter().sum::<f64>() / n as f64;
    let var = v.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / n as f64;
    Stat {
        median: pct(&v, 50.0),
        p95: pct(&v, 95.0),
        p99: pct(&v, 99.0),
        min: *v.first().unwrap_or(&0.0),
        max: *v.last().unwrap_or(&0.0),
        stddev: var.sqrt(),
        mean,
        n: v.len(),
    }
}

fn stat_json(s: &Stat) -> Value {
    json!({
        "median_us": round2(s.median), "p95_us": round2(s.p95), "p99_us": round2(s.p99),
        "min_us": round2(s.min), "max_us": round2(s.max), "stddev_us": round2(s.stddev),
        "mean_us": round2(s.mean), "n": s.n,
    })
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

// ── Production-faithful query replicas (logic copied verbatim from index/mod.rs) ─
fn is_structural(query: &str) -> bool {
    query
        .chars()
        .any(|c| !c.is_alphanumeric() && !c.is_whitespace() && c != '_')
}

fn sanitize_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|tok| {
            tok.chars()
                .filter(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" ")
}

fn structural_snippet(content: &str, needle: &str) -> String {
    content
        .lines()
        .find(|l| l.contains(needle))
        .unwrap_or("")
        .trim()
        .chars()
        .take(120)
        .collect()
}

/// A comparable hit: (path, snippet, score formatted to 6 dp). Used both to time
/// `execute` and to assert byte-identical output across configs and vs production.
type Hit = (String, String, String);

fn run_query(conn: &Connection, query: &str) -> Vec<Hit> {
    if is_structural(query) {
        let q = query.trim();
        if q.is_empty() {
            return vec![];
        }
        if q.chars().count() >= 3 {
            let phrase = format!("\"{}\"", q.replace('"', "\"\""));
            let mut stmt = conn.prepare(TRIGRAM_SQL).unwrap();
            let rows = stmt
                .query_map(params![phrase, LIMIT as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .unwrap();
            rows.flatten()
                .map(|(p, c)| (p, structural_snippet(&c, q), "0.000000".to_string()))
                .collect()
        } else {
            let like = format!("%{q}%");
            let mut stmt = conn.prepare(LIKE_SQL).unwrap();
            let rows = stmt
                .query_map(params![like, LIMIT as i64], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })
                .unwrap();
            rows.flatten()
                .map(|(p, c)| (p, structural_snippet(&c, q), "0.000000".to_string()))
                .collect()
        }
    } else {
        let m = sanitize_query(query);
        if m.is_empty() {
            return vec![];
        }
        let mut stmt = conn.prepare(RANKED_SQL).unwrap();
        let rows = stmt
            .query_map(params![m, LIMIT as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                ))
            })
            .unwrap();
        rows.flatten()
            .map(|(p, s, sc)| (p, s, format!("{sc:.6}")))
            .collect()
    }
}

fn replica_all(conn: &Connection, queries: &[&str]) -> Vec<(String, Vec<Hit>)> {
    queries
        .iter()
        .map(|q| (q.to_string(), run_query(conn, q)))
        .collect()
}

fn search_to_tuples(resp: &SearchResponse) -> Vec<(String, Vec<Hit>)> {
    resp.results
        .iter()
        .map(|qr| {
            (
                qr.query.clone(),
                qr.hits
                    .iter()
                    .map(|h| (h.path.clone(), h.snippet.clone(), format!("{:.6}", h.score)))
                    .collect(),
            )
        })
        .collect()
}

// ── Corpus + index helpers ────────────────────────────────────────────────────
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

struct Built {
    _corpus: tempfile::TempDir,
    _data: tempfile::TempDir,
    corpus_path: PathBuf,
    db: PathBuf,
    idx: Index,
}

fn build_index(scale: usize) -> Built {
    let corpus = tempfile::tempdir().unwrap();
    scale_corpus(&corpus_root(), corpus.path(), scale);
    let data = tempfile::tempdir().unwrap();
    let idx = Index::open(data.path()).unwrap();
    idx.index_path(corpus.path(), true).unwrap();
    Built {
        corpus_path: corpus.path().to_path_buf(),
        db: data.path().join("index.db"),
        _corpus: corpus,
        _data: data,
        idx,
    }
}

fn open_cfg(db: &Path, extra: &[&str]) -> Connection {
    let conn = Connection::open(db).unwrap();
    configure_conn(&conn).unwrap(); // REAL production baseline (WAL + busy_handler)
    for p in extra {
        conn.execute_batch(p).unwrap();
    }
    conn
}

/// Open WITHOUT re-running `PRAGMA journal_mode=WAL`. The DB is persistently WAL (set
/// once in `Index::init`), so this connection still reads in WAL mode; the first query
/// triggers the wal-index attach lazily. Used only to attribute whether the per-op
/// configure cost is the pragma statement or the intrinsic first-touch attach.
fn open_nowal(db: &Path) -> Connection {
    let conn = Connection::open(db).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

fn all_queries() -> Vec<&'static str> {
    let mut v = RANKED_QUERIES.to_vec();
    v.extend_from_slice(STRUCTURAL_QUERIES);
    v
}

fn time_read_mix(conn: &Connection, queries: &[&str], iters: usize, warmup: usize) -> Vec<f64> {
    for _ in 0..warmup {
        for q in queries {
            black_box(run_query(conn, q));
        }
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        for q in queries {
            black_box(run_query(conn, q));
        }
        samples.push(t.elapsed().as_secs_f64() * 1e6);
    }
    samples
}

// ── W1: per-stage attribution ─────────────────────────────────────────────────
fn attribution(corpus: &Path, db: &Path, idx: &Index) -> BTreeMap<&'static str, Stat> {
    let mut out = BTreeMap::new();

    // repo_walk: the REAL file_manifest, run every search via ensure_index.
    let mut walk = Vec::new();
    for _ in 0..WARMUP {
        black_box(file_manifest(corpus));
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        black_box(file_manifest(corpus));
        walk.push(t.elapsed().as_secs_f64() * 1e6);
    }
    out.insert("1_repo_walk", stat(walk));

    // conn_open: a fresh Connection::open each iter (production opens per op).
    let mut open = Vec::new();
    for _ in 0..WARMUP {
        black_box(Connection::open(db).unwrap());
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        let c = Connection::open(db).unwrap();
        open.push(t.elapsed().as_secs_f64() * 1e6);
        black_box(c);
    }
    out.insert("2_conn_open", stat(open));

    // configure_conn: the REAL function on a freshly opened conn each iter.
    let mut cfg = Vec::new();
    for _ in 0..ITERS {
        let c = Connection::open(db).unwrap();
        let t = Instant::now();
        configure_conn(&c).unwrap();
        cfg.push(t.elapsed().as_secs_f64() * 1e6);
        black_box(c);
    }
    out.insert("3_configure_conn", stat(cfg));

    // prepare + execute on a warm configured conn, representative ranked query.
    let conn = open_cfg(db, &[]);
    let m = sanitize_query("config");
    let (mut prep, mut exec) = (Vec::new(), Vec::new());
    for _ in 0..WARMUP {
        let mut s = conn.prepare(RANKED_SQL).unwrap();
        let r: Vec<String> = s
            .query_map(params![m, LIMIT as i64], |row| row.get::<_, String>(0))
            .unwrap()
            .flatten()
            .collect();
        black_box(r);
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        let mut s = conn.prepare(RANKED_SQL).unwrap();
        prep.push(t.elapsed().as_secs_f64() * 1e6);
        let t = Instant::now();
        let r: Vec<(String, String, f64)> = s
            .query_map(params![m, LIMIT as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .flatten()
            .collect();
        exec.push(t.elapsed().as_secs_f64() * 1e6);
        black_box(r);
    }
    out.insert("4_prepare", stat(prep));
    out.insert("5_execute", stat(exec));

    // serialize: the REAL SearchResponse -> JSON (what the handler returns in-frame).
    let resp = idx.search(&["config".to_string()], LIMIT).unwrap();
    let mut ser = Vec::new();
    for _ in 0..WARMUP {
        black_box(serde_json::to_string(&resp).unwrap());
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        let s = serde_json::to_string(&resp).unwrap();
        ser.push(t.elapsed().as_secs_f64() * 1e6);
        black_box(s);
    }
    out.insert("6_serialize", stat(ser));

    out
}

// isolated `Index::search` (production config) and through-handler (walk+search+serialize).
fn end_to_end(corpus: &Path, idx: &Index, all_s: &[String]) -> (Stat, Stat) {
    for _ in 0..WARMUP {
        black_box(idx.search(all_s, LIMIT).unwrap());
    }
    let mut iso = Vec::new();
    for _ in 0..ITERS {
        let t = Instant::now();
        let r = idx.search(all_s, LIMIT).unwrap();
        iso.push(t.elapsed().as_secs_f64() * 1e6);
        black_box(r);
    }

    let stored = file_manifest(corpus); // the manifest ensure_index compares against
    for _ in 0..WARMUP {
        let w = file_manifest(corpus);
        let _ = w == stored;
        let r = idx.search(all_s, LIMIT).unwrap();
        black_box(serde_json::to_string(&r).unwrap());
    }
    let mut h = Vec::new();
    for _ in 0..ITERS {
        let t = Instant::now();
        let w = file_manifest(corpus);
        let eq = w == stored; // BTreeMap equality: the real fresh-index short-circuit
        let r = idx.search(all_s, LIMIT).unwrap();
        let s = serde_json::to_string(&r).unwrap();
        h.push(t.elapsed().as_secs_f64() * 1e6);
        black_box((eq, w, r, s));
    }
    (stat(iso), stat(h))
}

fn read_matrix(db: &Path, all: &[&str]) -> Vec<(String, Stat)> {
    READ_CONFIGS
        .iter()
        .map(|c| {
            let conn = open_cfg(db, c.extra);
            (c.name.to_string(), stat(time_read_mix(&conn, all, ITERS, WARMUP)))
        })
        .collect()
}

/// Read-path correctness gate: every config's output must be byte-identical to the
/// baseline config, and the baseline replica must equal the REAL `Index::search`.
fn correctness(db: &Path, all: &[&str], idx: &Index, all_s: &[String]) -> (bool, Vec<String>) {
    let mut notes = Vec::new();
    let base_conn = open_cfg(db, &[]);
    let base = replica_all(&base_conn, all);

    let prod = search_to_tuples(&idx.search(all_s, LIMIT).unwrap());
    let tie = base == prod;
    if !tie {
        notes.push("MISMATCH: benchmark replica != production Index::search".to_string());
    }

    let mut all_ok = tie;
    for c in READ_CONFIGS {
        let conn = open_cfg(db, c.extra);
        let r = replica_all(&conn, all);
        if r == base {
            notes.push(format!("identical: {}", c.name));
        } else {
            all_ok = false;
            notes.push(format!("MISMATCH under config: {}", c.name));
        }
    }
    (all_ok, notes)
}

fn noise_floor(db: &Path, all: &[&str]) -> (Stat, Stat) {
    let a = stat(time_read_mix(&open_cfg(db, &[]), all, ITERS, WARMUP));
    let b = stat(time_read_mix(&open_cfg(db, &[]), all, ITERS, WARMUP));
    (a, b)
}

/// Concurrency tail over the same `run_query` path, A/B by connection lifecycle:
/// `reuse=false` opens+configures a fresh connection per call (production today);
/// `reuse=true` opens once per thread and reuses it (what T4 would do). N = logical
/// CPUs. p99 is where conn-per-op WAL-index-attach contention (amplified by the 1ms
/// busy_handler) shows up and a mean would hide it.
fn concurrent(db: &Path, all: &[&str], n: usize, reuse: bool) -> Stat {
    std::thread::scope(|s| {
        let mut handles = Vec::new();
        for _ in 0..n {
            handles.push(s.spawn(move || {
                let held = if reuse { Some(open_cfg(db, &[])) } else { None };
                let run = |v: &mut Vec<f64>| {
                    let t = Instant::now();
                    match held.as_ref() {
                        Some(c) => {
                            for q in all {
                                black_box(run_query(c, q));
                            }
                        }
                        None => {
                            let c = open_cfg(db, &[]);
                            for q in all {
                                black_box(run_query(&c, q));
                            }
                        }
                    }
                    v.push(t.elapsed().as_secs_f64() * 1e6);
                };
                let mut warm = Vec::new();
                for _ in 0..10 {
                    run(&mut warm);
                }
                let mut v = Vec::with_capacity(CONC_PER_THREAD);
                for _ in 0..CONC_PER_THREAD {
                    run(&mut v);
                }
                v
            }));
        }
        let mut all_lat = Vec::new();
        for h in handles {
            all_lat.extend(h.join().unwrap());
        }
        stat(all_lat)
    })
}

/// T4 preview: measure exactly what connection reuse would save on the SAME query
/// path, so the delta is purely the connection lifecycle (the per-op WAL-index attach
/// + first-MATCH FTS5 vtable init, both one-time-per-connection). Sequential AND
/// concurrent. This is the evidence that promotes or rejects T4.
fn reuse_probe(db: &Path, all: &[&str], do_concurrent: bool) -> Value {
    let mut per_op = Vec::new();
    for _ in 0..WARMUP {
        let c = open_cfg(db, &[]);
        for q in all {
            black_box(run_query(&c, q));
        }
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        let c = open_cfg(db, &[]);
        for q in all {
            black_box(run_query(&c, q));
        }
        per_op.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let per_op = stat(per_op);
    let mut per_op_nowal = Vec::new();
    for _ in 0..WARMUP {
        let c = open_nowal(db);
        for q in all {
            black_box(run_query(&c, q));
        }
    }
    for _ in 0..ITERS {
        let t = Instant::now();
        let c = open_nowal(db);
        for q in all {
            black_box(run_query(&c, q));
        }
        per_op_nowal.push(t.elapsed().as_secs_f64() * 1e6);
    }
    let per_op_nowal = stat(per_op_nowal);
    let reused = stat(time_read_mix(&open_cfg(db, &[]), all, ITERS, WARMUP));
    let saved = per_op.median - reused.median;
    let saved_pct = if per_op.median > 0.0 {
        saved / per_op.median * 100.0
    } else {
        0.0
    };
    let pragma_cost = per_op.median - per_op_nowal.median;

    println!("\n### T4 preview - connection reuse on the same query path\n");
    stat_head();
    line("sequential per-op (open+cfg+query)", &per_op);
    line("sequential per-op NO wal-pragma", &per_op_nowal);
    line("sequential reused (query only)", &reused);
    println!("  -> reuse saves {saved:.2}us/call sequential ({saved_pct:.1}% of per-op)");
    println!(
        "  -> skipping the journal_mode=WAL pragma alone saves {pragma_cost:.2}us/call (rest is the intrinsic first-touch attach + vtab init, only reuse amortizes it)"
    );

    let mut out = json!({
        "sequential_per_op_us": stat_json(&per_op),
        "sequential_per_op_nowal_us": stat_json(&per_op_nowal),
        "sequential_reused_us": stat_json(&reused),
        "sequential_saved_us": round2(saved),
        "sequential_saved_pct": round2(saved_pct),
        "wal_pragma_cost_us": round2(pragma_cost),
    });

    if do_concurrent {
        let n = std::thread::available_parallelism()
            .map(|x| x.get())
            .unwrap_or(1);
        let c_perop = concurrent(db, all, n, false);
        let c_reuse = concurrent(db, all, n, true);
        line(&format!("concurrent per-op (x{n})"), &c_perop);
        line(&format!("concurrent reused (x{n})"), &c_reuse);
        println!(
            "  -> concurrent p99: per-op {:.0}us vs reused {:.0}us  ({:.1}x). Reuse not fixing it",
            c_perop.p99,
            c_reuse.p99,
            if c_reuse.p99 > 0.0 {
                c_perop.p99 / c_reuse.p99
            } else {
                0.0
            }
        );
        println!("     => the concurrent tail is query-execution contention, NOT connection-open.");
        out["threads"] = json!(n);
        out["concurrent_per_op_us"] = stat_json(&c_perop);
        out["concurrent_reused_us"] = stat_json(&c_reuse);
    }
    out
}

// ── W2: incremental re-index / edit loop ──────────────────────────────────────
fn touch(path: &Path, content: &[u8], k: usize) {
    std::fs::write(path, content).unwrap();
    let mt = SystemTime::now() + Duration::from_secs((k as u64) + 1);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(mt)
        .unwrap();
}

/// Real edit-loop latency: touch one file (identical bytes, bumped mtime so it is the
/// single changed file), call the REAL `index_path`, repeat. Includes the walk - a real
/// edit pays it via ensure_index. Returns (stat_us, files_read_observed).
fn w2_edit_loop(corpus: &Path, idx: &Index) -> (Stat, usize) {
    let target = corpus.join("payments.rs");
    let content = std::fs::read(&target).unwrap();
    for k in 0..WARMUP {
        touch(&target, &content, k);
        black_box(idx.index_path(corpus, true).unwrap());
    }
    let mut v = Vec::new();
    let mut files_read = 0;
    for k in 0..ITERS {
        touch(&target, &content, WARMUP + k);
        let t = Instant::now();
        let r = idx.index_path(corpus, true).unwrap();
        v.push(t.elapsed().as_secs_f64() * 1e6);
        files_read = r.files_read;
    }
    (stat(v), files_read)
}

/// Write-path PRAGMA attribution: per-commit latency of one file's re-index transaction
/// (DELETE + re-INSERT the same rows + COMMIT) under each write config. Autocheckpoint
/// is disabled so a stray WAL checkpoint doesn't inflate one config (disclosed).
fn w2_write_matrix(db: &Path) -> Vec<(String, Stat)> {
    let probe = open_cfg(db, &[]);
    let path: String = probe
        .query_row(
            "SELECT path FROM chunks GROUP BY path ORDER BY count(*) DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let rows: Vec<(String, String, String)> = {
        let mut s = probe
            .prepare("SELECT chunk_id, symbols, content FROM chunks WHERE path = ?1")
            .unwrap();
        s.query_map(params![path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .flatten()
            .collect()
    };
    drop(probe);

    WRITE_CONFIGS
        .iter()
        .map(|c| {
            let mut conn = open_cfg(db, c.extra);
            conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
            let run_tx = |conn: &mut Connection| {
                let tx = conn.transaction().unwrap();
                tx.execute("DELETE FROM chunks WHERE path = ?1", params![path])
                    .unwrap();
                tx.execute("DELETE FROM chunks_tri WHERE path = ?1", params![path])
                    .unwrap();
                for (cid, sym, cont) in &rows {
                    tx.execute(
                        "INSERT INTO chunks (path, chunk_id, symbols, content) VALUES (?1,?2,?3,?4)",
                        params![path, cid, sym, cont],
                    )
                    .unwrap();
                    tx.execute(
                        "INSERT INTO chunks_tri (path, chunk_id, content) VALUES (?1,?2,?3)",
                        params![path, cid, cont],
                    )
                    .unwrap();
                }
                tx.commit().unwrap();
            };
            for _ in 0..WARMUP {
                run_tx(&mut conn);
            }
            let mut v = Vec::new();
            for _ in 0..ITERS {
                let t = Instant::now();
                run_tx(&mut conn);
                v.push(t.elapsed().as_secs_f64() * 1e6);
            }
            (c.name.to_string(), stat(v))
        })
        .collect()
}

// ── W3: cold full build + post-fragmentation optimize ─────────────────────────
fn w3_cold_build(scale: usize) -> f64 {
    let corpus = tempfile::tempdir().unwrap();
    scale_corpus(&corpus_root(), corpus.path(), scale);
    let data = tempfile::tempdir().unwrap();
    let t = Instant::now();
    let idx = Index::open(data.path()).unwrap();
    idx.index_path(corpus.path(), true).unwrap();
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    black_box(idx.chunk_count().unwrap());
    ms
}

/// Fragment the FTS5 segment forest with many small writes, measure read latency,
/// run `optimize`, measure again. On an already-compact index the delta is ~0 and
/// reported as such. Returns (before, after, optimize_ms).
fn w3_optimize(all: &[&str]) -> (Stat, Stat, f64) {
    let b = build_index(W2_SCALE);
    let files = ["payments.rs", "scheduler.rs", "session.rs", "config.rs", "db.rs"];
    for round in 0..FRAG_ROUNDS {
        let f = b.corpus_path.join(files[round % files.len()]);
        let c = std::fs::read(&f).unwrap();
        touch(&f, &c, round);
        b.idx.index_path(&b.corpus_path, true).unwrap();
    }
    let before = stat(time_read_mix(&open_cfg(&b.db, &[]), all, ITERS, WARMUP));
    let t = Instant::now();
    {
        let oc = open_cfg(&b.db, &[]);
        oc.execute_batch(
            "INSERT INTO chunks(chunks) VALUES('optimize'); INSERT INTO chunks_tri(chunks_tri) VALUES('optimize');",
        )
        .unwrap();
    }
    let opt_ms = t.elapsed().as_secs_f64() * 1000.0;
    let after = stat(time_read_mix(&open_cfg(&b.db, &[]), all, ITERS, WARMUP));
    (before, after, opt_ms)
}

/// T6's actual trigger scenario: a COLD bulk build (one big index_path, no incremental
/// fragmentation). Does optimizing the segments a from-scratch build leaves help reads,
/// or is one transaction already compact? Decides whether T6 is a win or a no-op.
fn w3_cold_optimize(all: &[&str]) -> (Stat, Stat, f64) {
    let b = build_index(W2_SCALE);
    let before = stat(time_read_mix(&open_cfg(&b.db, &[]), all, ITERS, WARMUP));
    let t = Instant::now();
    {
        let oc = open_cfg(&b.db, &[]);
        oc.execute_batch(
            "INSERT INTO chunks(chunks) VALUES('optimize'); INSERT INTO chunks_tri(chunks_tri) VALUES('optimize');",
        )
        .unwrap();
    }
    let opt_ms = t.elapsed().as_secs_f64() * 1000.0;
    let after = stat(time_read_mix(&open_cfg(&b.db, &[]), all, ITERS, WARMUP));
    (before, after, opt_ms)
}

// ── Reporting helpers ─────────────────────────────────────────────────────────
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
        "cache_state": "warm (steady-state) - cold-cache requires `sudo purge`, NOT run -> warm-only",
        "unix_time": ts,
        "iters": ITERS,
        "warmup": WARMUP,
        "scales": SCALES,
        "ranked_queries": RANKED_QUERIES,
        "structural_queries": STRUCTURAL_QUERIES,
    })
}

fn line(label: &str, s: &Stat) {
    println!(
        "| {label:<34} | {:>9.2} | {:>9.2} | {:>9.2} | {:>9.2} | {:>9.2} |",
        s.median, s.p95, s.p99, s.stddev, s.max
    );
}

fn stat_head() {
    println!(
        "| {:<34} | {:>9} | {:>9} | {:>9} | {:>9} | {:>9} |",
        "(microseconds)", "median", "p95", "p99", "stddev", "max"
    );
    println!("|{0:-<36}|{0:-<11}|{0:-<11}|{0:-<11}|{0:-<11}|{0:-<11}|", "");
}

fn main() {
    let mi = machine_info();
    println!("# bench_tuning - honest SQLite/FTS5 attribution + tuning matrix\n");
    println!(
        "machine: {} / {} / {} logical CPUs",
        mi["os"], mi["arch"], mi["logical_cpus"]
    );
    println!("cache state: {}", mi["cache_state"].as_str().unwrap());
    println!(
        "params: iters={ITERS} warmup={WARMUP} limit={LIMIT}  scales={SCALES:?}\n"
    );
    println!("DISCLOSURES:");
    println!("  - Replicated fixtures over-represent duplicate trigrams; index-size / cache");
    println!("    numbers from 10x/50x are an UPPER bound on a cache effect, not representative.");
    println!("  - through-handler arm = real walk + real Index::search + real serialize; it");
    println!("    EXCLUDES the ops side-log, the rmcp Json/Parameters newtypes, and the tokio");
    println!("    task spawn (all sub-microsecond; lens_search is crate-private).");
    println!("  - W2 write matrix disables wal_autocheckpoint so a stray checkpoint can't");
    println!("    inflate one config; durability of synchronous=NORMAL discussed in FINDINGS.");
    println!("  - gating is within-run A/B vs the A-vs-A noise floor, never absolute ms.\n");

    // Prove what the REAL production configure_conn applies (T3 verification).
    {
        let tmp = tempfile::tempdir().unwrap();
        let _ = Index::open(tmp.path()).unwrap();
        let c = Connection::open(tmp.path().join("index.db")).unwrap();
        configure_conn(&c).unwrap();
        let jmode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        println!("production configure_conn => journal_mode={jmode}, synchronous={sync} (2=FULL, 1=NORMAL, 0=OFF)\n");
    }

    let all = all_queries();
    let all_s: Vec<String> = all.iter().map(|s| s.to_string()).collect();

    let mut scales_json = Map::new();
    let mut correctness_ok = true;
    let mut correctness_notes: Vec<String> = Vec::new();
    let largest = *SCALES.last().unwrap();

    for &scale in SCALES {
        let b = build_index(scale);
        let chunks = b.idx.chunk_count().unwrap();
        let nfiles = file_manifest(&b.corpus_path).len();
        println!("\n## scale {scale}x  ({nfiles} files, {chunks} chunks)\n");

        // Attribution.
        let attr = attribution(&b.corpus_path, &b.db, &b.idx);
        println!("### per-stage attribution (one search call, by stage)\n");
        stat_head();
        for (k, s) in &attr {
            line(k, s);
        }
        let walk_med = attr["1_repo_walk"].median;
        let open_med = attr["2_conn_open"].median;
        println!(
            "\n  -> repo_walk median {:.2}us vs conn_open median {:.2}us  =>  walk is {:.1}x conn_open",
            walk_med,
            open_med,
            if open_med > 0.0 { walk_med / open_med } else { 0.0 }
        );

        // End to end.
        let (iso, handler) = end_to_end(&b.corpus_path, &b.idx, &all_s);
        println!("\n### end-to-end (full {}-query mix)\n", all.len());
        stat_head();
        line("isolated Index::search", &iso);
        line("through-handler (walk+search+ser)", &handler);

        // Read config matrix.
        let matrix = read_matrix(&b.db, &all);
        let base_med = matrix[0].1.median;
        println!("\n### read config matrix (query-only, warm conn; delta vs baseline)\n");
        stat_head();
        let mut matrix_json = Vec::new();
        for (name, s) in &matrix {
            line(name, s);
            let delta = s.median - base_med;
            let pctd = if base_med > 0.0 { delta / base_med * 100.0 } else { 0.0 };
            println!("|   -> delta {delta:+.2}us ({pctd:+.1}%)");
            matrix_json.push(json!({"config": name, "stat": stat_json(s),
                "delta_us": round2(delta), "delta_pct": round2(pctd)}));
        }

        // Noise floor.
        let (nfa, nfb) = noise_floor(&b.db, &all);
        let floor = (nfa.median - nfb.median).abs();
        println!(
            "\n### A-vs-A noise floor: median A {:.2}us, A' {:.2}us  =>  floor |delta| = {:.2}us (stddev A {:.2}, A' {:.2})",
            nfa.median, nfb.median, floor, nfa.stddev, nfb.stddev
        );
        println!("    (any config delta within +-{floor:.2}us is NOT a measurable difference)");

        // Correctness.
        let (cok, cnotes) = correctness(&b.db, &all, &b.idx, &all_s);
        correctness_ok &= cok;
        println!(
            "\n### read-path correctness across configs: {}",
            if cok { "PASS (byte-identical)" } else { "FAIL" }
        );
        if scale == SCALES[0] {
            correctness_notes = cnotes;
        }

        // T4-preview reuse probe at every scale (concurrent A/B only at largest, since
        // it proved connection-independent).
        let reuse_json = reuse_probe(&b.db, &all, scale == largest);

        let attr_json: Map<String, Value> =
            attr.iter().map(|(k, s)| (k.to_string(), stat_json(s))).collect();
        scales_json.insert(
            scale.to_string(),
            json!({
                "files": nfiles, "chunks": chunks,
                "attribution_us": attr_json,
                "isolated_search": stat_json(&iso),
                "through_handler": stat_json(&handler),
                "read_config_matrix": matrix_json,
                "noise_floor_us": round2(floor),
                "noise_a_median_us": round2(nfa.median),
                "noise_aprime_median_us": round2(nfb.median),
                "correctness_pass": cok,
                "reuse_probe": reuse_json,
            }),
        );
    }

    // ── W2: edit loop + write matrix ─────────────────────────────────────────
    println!("\n## W2 - incremental re-index / edit loop (scale {W2_SCALE}x)\n");
    let b2 = build_index(W2_SCALE);
    let (edit, files_read) = w2_edit_loop(&b2.corpus_path, &b2.idx);
    println!(
        "real edit-loop (touch 1 file -> index_path; files_read={files_read}): median {:.2}us, p95 {:.2}us, p99 {:.2}us",
        edit.median, edit.p95, edit.p99
    );
    println!("\n### write config matrix - per-commit re-index latency (delta vs baseline)\n");
    stat_head();
    let wm = w2_write_matrix(&b2.db);
    let wbase = wm[0].1.median;
    let mut wm_json = Vec::new();
    for (name, s) in &wm {
        line(name, s);
        let delta = s.median - wbase;
        let pctd = if wbase > 0.0 { delta / wbase * 100.0 } else { 0.0 };
        println!("|   -> delta {delta:+.2}us ({pctd:+.1}%)");
        wm_json.push(json!({"config": name, "stat": stat_json(s),
            "delta_us": round2(delta), "delta_pct": round2(pctd)}));
    }

    // ── W3: cold build + optimize ────────────────────────────────────────────
    println!("\n## W3 - cold full build + post-fragmentation optimize\n");
    let cold_ms = w3_cold_build(W2_SCALE);
    println!("cold full build (scale {W2_SCALE}x, warm-only): {cold_ms:.1}ms");
    let (before, after, opt_ms) = w3_optimize(&all);
    let qd = before.median - after.median;
    let qpct = if before.median > 0.0 { qd / before.median * 100.0 } else { 0.0 };
    println!(
        "fragmented ({FRAG_ROUNDS} small writes) read median {:.2}us -> after optimize {:.2}us  (delta {:+.2}us, {:+.1}%); optimize cost {:.1}ms",
        before.median, after.median, qd, qpct, opt_ms
    );
    let (cb_before, cb_after, cb_opt_ms) = w3_cold_optimize(&all);
    let cbd = cb_before.median - cb_after.median;
    let cbpct = if cb_before.median > 0.0 { cbd / cb_before.median * 100.0 } else { 0.0 };
    println!(
        "cold bulk build (T6 trigger) read median {:.2}us -> after optimize {:.2}us  (delta {:+.2}us, {:+.1}%); optimize cost {:.1}ms",
        cb_before.median, cb_after.median, cbd, cbpct, cb_opt_ms
    );

    // ── Optional report-only real corpus (never gated) ───────────────────────
    let real_corpus_json = if let Ok(p) = std::env::var("LENS_BENCH_CORPUS") {
        let path = PathBuf::from(&p);
        if path.is_dir() {
            println!("\n## real corpus (LENS_BENCH_CORPUS={p}) - REPORT ONLY, not gated\n");
            let data = tempfile::tempdir().unwrap();
            let idx = Index::open(data.path()).unwrap();
            idx.index_path(&path, true).unwrap();
            let db = data.path().join("index.db");
            let (iso, handler) = end_to_end(&path, &idx, &all_s);
            stat_head();
            line("isolated Index::search", &iso);
            line("through-handler", &handler);
            let matrix = read_matrix(&db, &all);
            for (name, s) in &matrix {
                line(name, s);
            }
            Some(json!({
                "path": p,
                "files": file_manifest(&path).len(),
                "isolated_search": stat_json(&iso),
                "through_handler": stat_json(&handler),
            }))
        } else {
            None
        }
    } else {
        None
    };

    // ── Write baseline.json ──────────────────────────────────────────────────
    let out = json!({
        "machine": mi,
        "scales": scales_json,
        "w2_edit_loop_us": stat_json(&edit),
        "w2_files_read": files_read,
        "w2_write_matrix": wm_json,
        "w3_cold_build_ms": round2(cold_ms),
        "w3_fragmented_read_us": stat_json(&before),
        "w3_optimized_read_us": stat_json(&after),
        "w3_optimize_cost_ms": round2(opt_ms),
        "w3_cold_bulk_read_us": stat_json(&cb_before),
        "w3_cold_bulk_optimized_read_us": stat_json(&cb_after),
        "w3_cold_bulk_optimize_cost_ms": round2(cb_opt_ms),
        "real_corpus": real_corpus_json,
        "correctness_pass": correctness_ok,
        "correctness_notes": correctness_notes,
    });
    let results_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/tuning/results");
    std::fs::create_dir_all(&results_dir).unwrap();
    let baseline = results_dir.join("baseline.json");
    std::fs::write(&baseline, serde_json::to_string_pretty(&out).unwrap() + "\n").unwrap();
    println!("\nwrote {}", baseline.display());

    println!(
        "\nread-path correctness across all configs/scales: {}",
        if correctness_ok { "PASS" } else { "FAIL" }
    );
    if !correctness_ok {
        eprintln!("bench_tuning: FAIL - a tuning config changed search output (correctness regression)");
        std::process::exit(1);
    }
}
