//! Concurrency gate for the FTS index writer (Bug A: "database is locked").
//!
//! `index_path` used to hold the `index.db` write lock for an entire repo walk
//! (one transaction opened before the walk, committed after it). A second writer
//! (the MCP server's warmup/auto-index, or a concurrent `lens index`) then spun
//! on the busy handler until its ceiling and got `SQLITE_BUSY`. Batching the
//! commits (release the lock every ~150 files) lets concurrent writers interleave.
//!
//! This file holds exactly one test so its `LENS_BUSY_MS` env set is not racing
//! other tests in the same binary.

use lens::index::Index;
use std::thread;

/// Files per writer: a few COMMIT_BATCH-sized batches. Enough that two writers
/// started together overlap their write phases (pre-fix, the second writer's deferred
/// write tries to upgrade past the first's lock and SQLite returns SQLITE_BUSY
/// *without* the busy handler, so it fails instantly (RED). Small enough that
/// post-fix the brief, lock-free read phase of each batch lets the other writer
/// interleave well within the 200 ms ceiling rather than starving over many rounds.
const FILES_PER_WRITER: usize = 400;

/// Lines per file: a single short function (one chunk), keeping per-file work low so
/// a 150-file batch commits quickly.
const LINES_PER_FILE: usize = 4;

fn write_corpus(dir: &std::path::Path, prefix: &str) {
    for i in 0..FILES_PER_WRITER {
        let mut body = String::with_capacity(LINES_PER_FILE * 48);
        for l in 0..LINES_PER_FILE {
            // A distinct symbol per line so chunk_symbols does real work too.
            body.push_str(&format!(
                "fn {prefix}_fn_{i}_{l}() {{ let {prefix}_v_{i}_{l} = {l}; }}\n"
            ));
        }
        std::fs::write(dir.join(format!("{prefix}_{i}.rs")), body).unwrap();
    }
}

#[test]
fn concurrent_writers_no_lock() {
    // A short busy ceiling: a writer that cannot acquire the lock within 200 ms of
    // retries gives up with SQLITE_BUSY. Must be set before any connection opens.
    std::env::set_var("LENS_BUSY_MS", "200");

    let data = tempfile::tempdir().unwrap();
    let corpus_a = tempfile::tempdir().unwrap();
    let corpus_b = tempfile::tempdir().unwrap();
    write_corpus(corpus_a.path(), "a");
    write_corpus(corpus_b.path(), "b");

    // Both writers share one index.db (same data dir). `Index` is just a path, so
    // each clone opens its own connection: the real two-writer scenario.
    let idx = Index::open(data.path()).unwrap();
    let (idx_a, idx_b) = (idx.clone(), idx.clone());
    let (root_a, root_b) = (
        corpus_a.path().to_path_buf(),
        corpus_b.path().to_path_buf(),
    );

    let ha = thread::spawn(move || idx_a.index_path(&root_a, true));
    let hb = thread::spawn(move || idx_b.index_path(&root_b, true));
    let ra = ha.join().expect("writer A panicked");
    let rb = hb.join().expect("writer B panicked");

    std::env::remove_var("LENS_BUSY_MS");

    assert!(
        ra.is_ok(),
        "writer A got a lock error (the bug): {:?}",
        ra.err()
    );
    assert!(
        rb.is_ok(),
        "writer B got a lock error (the bug): {:?}",
        rb.err()
    );

    // Both corpora must be fully indexed — batched commits must not drop work.
    let total = idx.chunk_count().unwrap();
    let expected = (2 * FILES_PER_WRITER) as i64; // >= 1 chunk per file, both corpora
    assert!(
        total >= expected,
        "expected >= {expected} chunks across both corpora, got {total}"
    );
}
