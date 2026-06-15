//! Integration tests for FTS5 index + search.

use ctxforge::index::Index;
use std::fs;
use tempfile::tempdir;

#[test]
fn index_then_search_roundtrip() {
    let data = tempdir().unwrap();
    let corpus = tempdir().unwrap();
    fs::write(
        corpus.path().join("server.rs"),
        "fn handle_request() {\n    // route the inbound request\n    dispatch();\n}\n",
    )
    .unwrap();
    fs::write(
        corpus.path().join("util.rs"),
        "fn checksum(bytes: &[u8]) -> u32 { 0 }\n",
    )
    .unwrap();

    let idx = Index::open(data.path()).unwrap();
    let report = idx.index_path(corpus.path(), true).unwrap();
    assert!(report.files_indexed >= 2);

    let out = idx
        .search(&["request".into(), "checksum".into()], 5)
        .unwrap();
    assert_eq!(out.results.len(), 2);
    assert!(out.results[0].hits[0].path.ends_with("server.rs"));
    assert!(out.results[1].hits[0].path.ends_with("util.rs"));
    assert!(!out.results[0].hits[0].snippet.is_empty());
}
