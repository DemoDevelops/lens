//! Integration test: build the graph over `tests/fixtures/md/` and assert the
//! node/edge spec documented in `index.md`'s top comment (headings, section
//! `contains` nesting, and cross-doc link `imports` edges).

use lens::discovery;
use std::path::Path;

fn heading_id(graph: &discovery::graph::Graph, name: &str, file_suffix: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|n| n.kind == "heading" && n.name == name && n.file.ends_with(file_suffix))
        .unwrap_or_else(|| panic!("missing heading node {name:?} in {file_suffix:?}"))
        .id
        .clone()
}

fn has_edge(graph: &discovery::graph::Graph, from: &str, to: &str, kind: &str) -> bool {
    graph
        .edges
        .iter()
        .any(|e| e.from == from && e.to == to && e.kind == kind)
}

#[test]
fn markdown_fixture_graph_matches_spec() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/md");
    let graph = discovery::discover(&root, None).unwrap().graph;

    // 1. Heading nodes exist for Index, Setup, Local, Remote in index.md.
    let index_id = heading_id(&graph, "Index", "index.md");
    let setup_id = heading_id(&graph, "Setup", "index.md");
    let local_id = heading_id(&graph, "Local", "index.md");
    let remote_id = heading_id(&graph, "Remote", "index.md");

    // 2. `contains` edges reflect section nesting: Index>Setup, Setup>Local,
    // Index>Remote -- but NOT Index>Local (Local nests under Setup, not Index).
    assert!(
        has_edge(&graph, &index_id, &setup_id, "contains"),
        "expected Index contains Setup"
    );
    assert!(
        has_edge(&graph, &setup_id, &local_id, "contains"),
        "expected Setup contains Local"
    );
    assert!(
        has_edge(&graph, &index_id, &remote_id, "contains"),
        "expected Index contains Remote"
    );
    assert!(
        !has_edge(&graph, &index_id, &local_id, "contains"),
        "Index must NOT directly contain Local"
    );

    // 3. index.md's module node has outgoing `imports` edges resolving to real
    // nodes named `deploy` and `arch` (the link targets, synthetic import nodes
    // since no node is literally named `deploy`/`arch`).
    let index_module = graph
        .nodes
        .iter()
        .find(|n| n.kind == "module" && n.file.ends_with("index.md"))
        .unwrap_or_else(|| panic!("missing module node for index.md"));

    let import_targets: Vec<&str> = graph
        .edges
        .iter()
        .filter(|e| e.from == index_module.id && e.kind == "imports")
        .filter_map(|e| graph.node(&e.to))
        .map(|n| n.name.as_str())
        .collect();

    assert!(
        import_targets.contains(&"deploy"),
        "expected index.md module to import `deploy`, got {import_targets:?}"
    );
    assert!(
        import_targets.contains(&"arch"),
        "expected index.md module to import `arch`, got {import_targets:?}"
    );
}
