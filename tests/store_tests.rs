//! Integration tests for the reversible store and JSON compaction.

use lens::store::{compress, Store};
use serde_json::json;
use tempfile::tempdir;

#[test]
fn store_roundtrip_and_stats() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    let blob = "line\n".repeat(10_000);
    let reference = store.put(&blob).unwrap();
    assert_eq!(store.get(&reference).unwrap().unwrap(), blob);
    assert!(store.get("nonexistent").unwrap().is_none());

    store.bump_stat("calls", 2).unwrap();
    store.bump_stat("calls", 3).unwrap();
    assert_eq!(store.get_stat("calls").unwrap(), 5);
}

#[test]
fn compaction_roundtrips_through_store() {
    let dir = tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();

    let original = json!({
        "nodes": (0..40).map(|i| json!({
            "kind": "function",
            "file": "src/discovery/extract.rs",
            "name": format!("symbol_{i}")
        })).collect::<Vec<_>>()
    });

    // Store the plain original, return the compact form.
    let reference = store.put(&original.to_string()).unwrap();
    let compact = compress::compact_json(&original);

    // compact form is smaller and reverses back to the original.
    assert!(serde_json::to_string(&compact).unwrap().len() < original.to_string().len());
    assert_eq!(compress::expand_json(&compact), original);

    // retrieve recovers the byte-exact original.
    let recovered = store.get(&reference).unwrap().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&recovered).unwrap();
    assert_eq!(parsed, original);
}
