//! Integration tests for FTS5 index + search.

use lens::index::Index;
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

#[test]
fn pascalcase_subword_is_searchable() {
    let data = tempdir().unwrap();
    let corpus = tempdir().unwrap();
    // Both identifiers are compound PascalCase with NO snake_case sibling and NO
    // prose occurrence of "subscription"/"billing". On an unpatched build the
    // subword "Subscription" returns zero hits, and the function-declared
    // component "ResolveBillingState" is invisible to the symbols column.
    fs::write(
        corpus.path().join("screen.tsx"),
        "struct ConfirmSubscriptionScreen { id: u32 }\nexport function ResolveBillingState() {}\n",
    )
    .unwrap();

    let idx = Index::open(data.path()).unwrap();
    idx.index_path(corpus.path(), true).unwrap();

    // Subword of a PascalCase struct name is now findable (FAILS unpatched).
    let sub = idx.search(&["Subscription".into()], 5).unwrap();
    assert!(
        !sub.results[0].hits.is_empty(),
        "subword 'Subscription' must hit the fixture"
    );
    assert!(sub.results[0].hits[0].path.ends_with("screen.tsx"));

    // Subword of a function-declared component proves the keyword-capture addition.
    let bill = idx.search(&["Billing".into()], 5).unwrap();
    assert!(
        !bill.results[0].hits.is_empty(),
        "subword 'Billing' must hit the fixture"
    );
    assert!(bill.results[0].hits[0].path.ends_with("screen.tsx"));

    // The exact full identifier still ranks the defining file #1.
    let exact = idx.search(&["ConfirmSubscriptionScreen".into()], 5).unwrap();
    assert!(!exact.results[0].hits.is_empty());
    assert!(exact.results[0].hits[0].path.ends_with("screen.tsx"));
}

#[test]
fn long_token_hash_is_searchable() {
    // Regression guard: Tantivy's built-in `en_stem` includes `RemoveLongFilter(40)`,
    // which DROPS any token >= 40 bytes at index AND query time; FTS5's porter never
    // dropped long tokens. A 64-char sha256 (one contiguous token, so subword
    // splitting cannot rescue it) must stay searchable, like it was under FTS5.
    let data = tempdir().unwrap();
    let corpus = tempdir().unwrap();
    let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; // 64 hex
    fs::write(
        corpus.path().join("lock.rs"),
        format!("const CHECKSUM: &str = \"{hash}\";\n"),
    )
    .unwrap();
    let idx = Index::open(data.path()).unwrap();
    idx.index_path(corpus.path(), true).unwrap();

    let out = idx.search(&[hash.into()], 5).unwrap();
    assert!(
        !out.results[0].hits.is_empty(),
        "64-char hash must be searchable (RemoveLongFilter(40) recall regression)"
    );
    assert!(out.results[0].hits[0].path.ends_with("lock.rs"));
}
