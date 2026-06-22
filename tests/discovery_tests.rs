//! Integration tests for discovery + graph traversal across languages.

use lens::discovery::{self, query};
use std::fs;
use tempfile::tempdir;

#[test]
fn multi_language_discovery() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("a.rs"),
        "fn helper() {}\nfn main() { helper(); }\n",
    )
    .unwrap();
    fs::write(
        dir.path().join("b.py"),
        "def helper():\n    pass\n\ndef main():\n    helper()\n",
    )
    .unwrap();

    let out = discovery::discover(dir.path(), None).unwrap();
    assert!(out.response.languages.contains(&"rust".to_string()));
    assert!(out.response.languages.contains(&"python".to_string()));
    assert!(out.response.nodes > 0);
    assert!(out.response.edges > 0);
}

#[test]
fn lens_symbol_and_path_on_discovered_repo() {
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("chain.rs"),
        "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\nfn island() {}\n",
    )
    .unwrap();

    let graph = discovery::discover(dir.path(), None).unwrap().graph;

    let view = query::query(&graph, "a", Some("function"), 10, &[]);
    assert!(view.nodes.iter().any(|n| n.name == "a"));

    let connected = query::path(&graph, "a", "c");
    assert!(connected.found);
    assert_eq!(connected.path.first().unwrap().name, "a");
    assert_eq!(connected.path.last().unwrap().name, "c");

    let disconnected = query::path(&graph, "a", "island");
    assert!(!disconnected.found);
}
