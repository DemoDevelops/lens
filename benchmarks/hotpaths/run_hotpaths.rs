//! Timing harness for the three lens hot paths: lens_map build, graph-query,
//! and lens_search. Reports baseline ms over lens's own src/ tree.
//!
//! Numbers are reported only; no regression gate.
//!
//! `cargo run --release --bin bench_hotpaths`

use std::path::Path;
use std::time::Instant;

use lens::discovery::discover;
use lens::index::schema::Index;

// Fixed query set for graph-query loop.
const GRAPH_QUERIES: &[&str] = &["discover", "index", "graph", "node", "search"];

// Fixed query set for FTS search loop.
const SEARCH_QUERIES: &[&str] = &["discover", "index_path", "Graph", "find_by_name", "shortest"];

const MAP_ITERS: usize = 5;
const GRAPH_N: usize = 10_000;
const SEARCH_N: usize = 500;

fn main() {
    let src = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));

    // Corpus stats.
    let rs_files: Vec<_> = walkdir(src);
    let file_count = rs_files.len();
    let loc: usize = rs_files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.lines().count())
        .sum();
    println!("# bench_hotpaths -- corpus: {file_count} files, {loc} LOC (lens/src/)");
    println!();

    // ── 1. lens_map build ───────────────────────────────────────────────────
    // Warm up once.
    let _ = discover(src, None).expect("discover warm-up");
    let t = Instant::now();
    for _ in 0..MAP_ITERS {
        let _ = discover(src, None).expect("discover");
    }
    let map_mean_ms = t.elapsed().as_secs_f64() * 1000.0 / MAP_ITERS as f64;

    // ── 2. graph-query ──────────────────────────────────────────────────────
    let outcome = discover(src, None).expect("discover for graph");
    let graph = outcome.graph;

    // Collect some node ids for shortest_path probes.
    let all_nodes = &graph.nodes;
    let node_a = all_nodes.first().map(|n| n.id.as_str()).unwrap_or("");
    let node_b = all_nodes.get(all_nodes.len() / 2).map(|n| n.id.as_str()).unwrap_or("");

    // Warm up.
    for i in 0..GRAPH_N {
        let q = GRAPH_QUERIES[i % GRAPH_QUERIES.len()];
        std::hint::black_box(graph.find_by_name(q, None));
    }

    let t = Instant::now();
    let mut sink = 0usize;
    for i in 0..GRAPH_N {
        let q = GRAPH_QUERIES[i % GRAPH_QUERIES.len()];
        // find_by_name
        sink += graph.find_by_name(q, None).len();
        // neighbors (depth 1)
        let (nodes, _) = graph.neighbors(node_a, 1);
        sink += nodes.len();
        // shortest_path
        sink += graph.shortest_path(node_a, node_b).map(|p| p.len()).unwrap_or(0);
    }
    std::hint::black_box(sink);
    let gq_total_ms = t.elapsed().as_secs_f64() * 1000.0;
    let gq_mean_us = gq_total_ms * 1000.0 / GRAPH_N as f64;

    // ── 3. lens_search ──────────────────────────────────────────────────────
    let idx_dir = tempfile::tempdir().expect("tempdir");
    let idx = Index::open(idx_dir.path()).expect("Index::open");

    // Build the index; time it.
    let t = Instant::now();
    idx.index_path(src, true).expect("index_path");
    let index_build_ms = t.elapsed().as_secs_f64() * 1000.0;

    // Warm up search.
    for _ in 0..10 {
        let qs: Vec<String> = SEARCH_QUERIES.iter().map(|s| s.to_string()).collect();
        std::hint::black_box(idx.search(&qs, 5).ok());
    }

    let t = Instant::now();
    let mut ssink = 0usize;
    for i in 0..SEARCH_N {
        let q = SEARCH_QUERIES[i % SEARCH_QUERIES.len()].to_string();
        if let Ok(r) = idx.search(&[q], 5) {
            ssink += r.results.iter().map(|qr| qr.hits.len()).sum::<usize>();
        }
    }
    std::hint::black_box(ssink);
    let search_total_ms = t.elapsed().as_secs_f64() * 1000.0;
    let search_mean_us = search_total_ms * 1000.0 / SEARCH_N as f64;

    // ── Report ──────────────────────────────────────────────────────────────
    println!("| path | metric | value |");
    println!("|---|---|---|");
    println!("| lens_map build | mean ms/build (N={MAP_ITERS}) | {map_mean_ms:.1} |");
    println!(
        "| graph-query | mean us/query (N={GRAPH_N}, 3 ops each) | {gq_mean_us:.2} |"
    );
    println!("| lens_search index build | total ms | {index_build_ms:.1} |");
    println!(
        "| lens_search query | mean us/query (N={SEARCH_N}) | {search_mean_us:.2} |"
    );
}

/// Collect all files under `dir` (respects .gitignore via ignore crate).
fn walkdir(dir: &Path) -> Vec<std::path::PathBuf> {
    use ignore::WalkBuilder;
    let mut out = Vec::new();
    let mut builder = WalkBuilder::new(dir);
    builder.standard_filters(true);
    for entry in builder.build().flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            out.push(entry.into_path());
        }
    }
    out
}
