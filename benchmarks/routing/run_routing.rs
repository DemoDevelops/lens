//! Microbenchmark for the routing hot path: command classification and the
//! per-(session,key) nudge throttle. Compares the rewrite (Aho-Corasick
//! `classify`, in-memory throttle) against the pre-rewrite baseline (the
//! regex-vec `is_structurally_bounded`, the filesystem-marker `guidance_once`),
//! captured by running this bench on the parent commit.
//!
//! `cargo run --release --bin bench_routing`

use std::time::Instant;

use lens::routing::{self, throttle};

/// Baselines from the pre-rewrite run of this bench (N=100_000, release).
const BASELINE_BOUNDED_NS: f64 = 291.8;
const BASELINE_ONCE_NS: f64 = 5002.2;

/// Fixed corpus exercising every path: bounded probes, unbounded read-only,
/// destructive roots, shell operators, build/network commands.
const CORPUS: &[&str] = &[
    "ls -la",
    "git log",
    "rm -rf /",
    "cat f | grep x",
    "find . -name '*.rs'",
    "pwd",
    "whoami",
    "git status",
    "git rev-parse HEAD",
    "node --version",
    "echo hello world",
    "grep -r foo src",
    "cargo build --release",
    "mkdir -p a/b/c",
    "cp a b",
    "mv x y",
    "git diff --stat",
    "ls -R /",
    "curl https://example.com",
    "python3 -c 'print(1)'",
];

const N: usize = 100_000;

fn main() {
    // ── classify / is_structurally_bounded: one Aho-Corasick pass ──────────
    // Warm up first: build the lazy automaton and page in the code so the timed
    // loop measures steady state. The very first post-build pass is ~1.5x slower
    // (CPU ramp + cold instruction cache) and would spuriously trip the assert.
    let mut sink = 0usize;
    for i in 0..N {
        sink += routing::is_structurally_bounded(CORPUS[i % CORPUS.len()]) as usize;
    }
    std::hint::black_box(sink);
    sink = 0;
    let t = Instant::now();
    for i in 0..N {
        if routing::is_structurally_bounded(CORPUS[i % CORPUS.len()]) {
            sink += 1;
        }
    }
    let bounded_ns = t.elapsed().as_nanos() as f64 / N as f64;
    std::hint::black_box(sink);

    // ── nudge throttle: steady-state membership check (per-call cost) ──────
    // After the one-time per-process load, `fired` is an in-memory cache hit;
    // that recurring cost is what the routing hot path pays.
    let dir = std::env::temp_dir().join(format!("lens-bench-throttle-{}", std::process::id()));
    throttle::mark(&dir, "bench", "k");
    for _ in 0..N {
        std::hint::black_box(throttle::fired(&dir, "bench", "k"));
    }
    let mut sink2 = 0usize;
    let t = Instant::now();
    for _ in 0..N {
        if throttle::fired(&dir, "bench", "k") {
            sink2 += 1;
        }
    }
    let throttle_ns = t.elapsed().as_nanos() as f64 / N as f64;
    std::hint::black_box(sink2);
    let _ = std::fs::remove_dir_all(&dir);

    let speedup = |base: f64, now: f64| if now > 0.0 { base / now } else { f64::INFINITY };
    println!("# routing rewrite vs baseline, N={N}");
    println!("| path | baseline ns/call | rewrite ns/call | speedup |");
    println!("|---|---|---|---|");
    println!(
        "| classify / is_structurally_bounded | {BASELINE_BOUNDED_NS:.1} | {bounded_ns:.1} | {:.1}x |",
        speedup(BASELINE_BOUNDED_NS, bounded_ns)
    );
    println!(
        "| nudge throttle (was guidance_once) | {BASELINE_ONCE_NS:.1} | {throttle_ns:.1} | {:.1}x |",
        speedup(BASELINE_ONCE_NS, throttle_ns)
    );

    assert!(
        bounded_ns <= BASELINE_BOUNDED_NS,
        "classify regressed: {bounded_ns:.1} > {BASELINE_BOUNDED_NS:.1} ns/call"
    );
    assert!(
        throttle_ns <= BASELINE_ONCE_NS,
        "throttle regressed: {throttle_ns:.1} > {BASELINE_ONCE_NS:.1} ns/call"
    );
    println!("\nOK: both paths within baseline.");
}
