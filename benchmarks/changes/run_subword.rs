//! `bench_subword` - frozen A/B benchmark of L28 (camelCase subword expansion in
//! `chunk_symbols`) measured honestly against the real lens search + symbol paths.
//!
//! For each labeled subword query (a Pascal/camel subword of a compound identifier
//! defined in exactly one fixture file):
//!
//!   lens_search: open a temp Index, index the frozen `fixtures/subword` corpus,
//!                run `index.search(&[query], 5)`. HIT = a top-5 hit whose path
//!                ends with the ground-truth file.
//!   lens_symbol: build the graph via `discovery::discover` and run the SAME
//!                substring-over-symbol-names path the `lens_symbol` MCP tool uses
//!                (`query::query(graph, name, None, limit, &[])`). HIT = the GT
//!                file among the top-5 symbol results.
//!
//! Prints per-query HIT booleans plus aggregate recall (overall and the
//! snakeCovered=false "pure Pascal" subset) for both arms. The search number is the
//! one L28 moves; the symbol number is identical on both worktrees (it does not use
//! `chunk_symbols`) and is reported as a control.
//!
//!   cargo run --bin bench_subword

use std::path::{Path, PathBuf};

use lens::discovery::{self, query as gquery};
use lens::index::Index;

fn bench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks")
}

fn corpus() -> PathBuf {
    bench_root().join("changes/fixtures/subword")
}

/// (query, ground-truth file, snakeCovered). snakeCovered = a snake_case sibling
/// already exposes the subword to the porter tokenizer today, so L28 is not the
/// only way that file could match the query.
const QUERIES: [(&str, &str, bool); 10] = [
    ("Subscription", "confirm_screen.tsx", false),
    ("Canceled", "confirm_screen.tsx", false),
    ("Billing", "billing.tsx", false),
    ("Portal", "billing.tsx", false),
    ("Selector", "payment.rs", false),
    ("Payment", "payment.rs", true),
    ("Socket", "websocket.rs", false),
    ("Connection", "websocket.rs", false),
    ("Validator", "auth.ts", false),
    ("Token", "auth.ts", true),
];

/// True when any path in `paths` ends with the ground-truth file at a path-component
/// boundary. The corpus is indexed by absolute path, so compare by basename/suffix
/// rather than equality.
fn hit(paths: &[String], gt: &str) -> bool {
    paths.iter().any(|p| {
        let p = p.replace('\\', "/");
        p == gt || p.ends_with(&format!("/{gt}"))
    })
}

/// Top-5 search-hit paths for `query` over the indexed corpus.
fn search_hits(index: &Index, query: &str) -> Vec<String> {
    let resp = index.search(&[query.to_string()], 5).unwrap();
    resp.results[0]
        .hits
        .iter()
        .map(|h| h.path.clone())
        .collect()
}

/// Top-5 symbol-result files, via the exact path the `lens_symbol` MCP tool uses:
/// `query::query(graph, name, None, limit, &[])` (empty recent-files = no proximity
/// boost), then the matched nodes' files in rank order. The view also carries the
/// matches' neighbors; we take the nodes whose name actually contains the query (the
/// matches), preserving the importance-then-id order `query` produced.
fn symbol_hits(graph: &discovery::graph::Graph, query: &str) -> Vec<String> {
    let view = gquery::query(graph, query, None, 5, &[]);
    let lq = query.to_ascii_lowercase();
    view.nodes
        .iter()
        .filter(|n| n.name.to_ascii_lowercase().contains(&lq))
        .map(|n| n.file.clone())
        .collect()
}

struct Row {
    query: &'static str,
    gt: &'static str,
    snake_covered: bool,
    search_hit: bool,
    symbol_hit: bool,
}

fn recall(rows: &[Row], hit: impl Fn(&Row) -> bool, keep: impl Fn(&Row) -> bool) -> (usize, usize) {
    let kept: Vec<&Row> = rows.iter().filter(|r| keep(r)).collect();
    let hits = kept.iter().filter(|r| hit(r)).count();
    (hits, kept.len())
}

fn frac(num: usize, den: usize) -> f64 {
    if den == 0 {
        0.0
    } else {
        num as f64 / den as f64
    }
}

fn main() {
    let dir = corpus();
    assert!(dir.exists(), "corpus missing: {}", dir.display());

    let data = tempfile::tempdir().unwrap();
    let index = Index::open(data.path()).unwrap();
    index.index_path(&dir, true).unwrap();

    let graph = discovery::discover(&dir, None).unwrap().graph;

    let mut rows: Vec<Row> = Vec::new();
    println!("# bench_subword - L28 subword recall (search) vs symbol control\n");
    println!("corpus: {}\n", dir.display());
    println!("| query | GT file | snakeCovered | searchHit | symbolHit |");
    println!("| --- | --- | :---: | :---: | :---: |");
    for (query, gt, snake_covered) in QUERIES {
        let search_hit = hit(&search_hits(&index, query), gt);
        let symbol_hit = hit(&symbol_hits(&graph, query), gt);
        println!(
            "| {query} | {gt} | {} | {} | {} |",
            snake_covered, search_hit, symbol_hit
        );
        rows.push(Row {
            query,
            gt,
            snake_covered,
            search_hit,
            symbol_hit,
        });
    }

    let (sh, sn) = recall(&rows, |r| r.search_hit, |_| true);
    let (sph, spn) = recall(&rows, |r| r.search_hit, |r| !r.snake_covered);
    let (yh, yn) = recall(&rows, |r| r.symbol_hit, |_| true);
    let (yph, ypn) = recall(&rows, |r| r.symbol_hit, |r| !r.snake_covered);

    println!("\n## Aggregate recall\n");
    println!(
        "lens_search overall: {sh}/{sn} = {:.3}",
        frac(sh, sn)
    );
    println!(
        "lens_search pure-Pascal (snakeCovered=false): {sph}/{spn} = {:.3}",
        frac(sph, spn)
    );
    println!("lens_symbol overall: {yh}/{yn} = {:.3}", frac(yh, yn));
    println!(
        "lens_symbol pure-Pascal (snakeCovered=false): {yph}/{ypn} = {:.3}",
        frac(yph, ypn)
    );

    // Machine-readable line for the A/B driver to diff between worktrees.
    println!(
        "\nMACHINE {}",
        rows.iter()
            .map(|r| format!(
                "{}:{}:{}:{}:{}",
                r.query,
                gt_base(r.gt),
                r.snake_covered as u8,
                r.search_hit as u8,
                r.symbol_hit as u8
            ))
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn gt_base(gt: &str) -> &str {
    Path::new(gt)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(gt)
}
