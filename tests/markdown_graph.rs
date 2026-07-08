//! Integration test: build the graph over `tests/fixtures/md/` and assert the
//! node/edge spec documented in `index.md`'s top comment -- headings, section
//! `contains` nesting, and the full cross-doc link resolution surface: real
//! module/heading targets (never bare-name stubs), duplicate-basename safety,
//! frontmatter title/tags/aliases, transclusion embeds, inline `#tag`s, and the
//! plain-CommonMark no-regression gate.

use lens::discovery;
use std::path::Path;

fn module_id(graph: &discovery::graph::Graph, file: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|n| n.kind == "module" && n.file == file)
        .unwrap_or_else(|| panic!("missing module node for {file:?}"))
        .id
        .clone()
}

fn heading_id(graph: &discovery::graph::Graph, name: &str, file: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|n| n.kind == "heading" && n.name == name && n.file == file)
        .unwrap_or_else(|| panic!("missing heading node {name:?} in {file:?}"))
        .id
        .clone()
}

fn tag_id(graph: &discovery::graph::Graph, name: &str) -> String {
    graph
        .nodes
        .iter()
        .find(|n| n.kind == "tag" && n.name == name)
        .unwrap_or_else(|| panic!("missing tag node {name:?}"))
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

    // Module nodes for every fixture, keyed by their (unique) rel path.
    let index_m = module_id(&graph, "index.md");
    let deploy_m = module_id(&graph, "deploy.md");
    let arch_m = module_id(&graph, "arch.md");
    let guide_m = module_id(&graph, "guide.md");
    let plain_m = module_id(&graph, "plain.md");
    let notes_m = module_id(&graph, "notes.md");
    let sub_util_m = module_id(&graph, "sub/util.md");
    let sub_index_m = module_id(&graph, "sub/index.md");
    assert_ne!(
        sub_index_m, index_m,
        "root index.md and sub/index.md must be distinct module nodes"
    );
    for id in [
        &index_m,
        &deploy_m,
        &arch_m,
        &guide_m,
        &plain_m,
        &notes_m,
        &sub_util_m,
        &sub_index_m,
    ] {
        assert_eq!(
            graph.node(id).unwrap().kind,
            "module",
            "expected a module node for id {id}"
        );
    }

    // 3. Inline + reference links resolve to REAL target module nodes, never a
    // synthetic bare-name stub.
    assert!(
        has_edge(&graph, &index_m, &deploy_m, "imports"),
        "index.md -[inline]-> deploy.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &sub_index_m, &sub_util_m, "imports"),
        "sub/index.md -[inline ./util.md]-> sub/util.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &sub_index_m, &guide_m, "imports"),
        "sub/index.md -[inline ../guide.md]-> guide.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &plain_m, &arch_m, "imports"),
        "plain.md -[inline]-> arch.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &guide_m, &deploy_m, "imports"),
        "guide.md -[reference deploy-ref]-> deploy.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &guide_m, &arch_m, "imports"),
        "guide.md -[reference arch-ref]-> arch.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &notes_m, &deploy_m, "imports"),
        "notes.md -[wikilink [[deploy]]]-> deploy.md missing; edges={:?}",
        graph.edges
    );
    assert!(
        has_edge(&graph, &notes_m, &arch_m, "imports"),
        "notes.md -[wikilink [[arch]]]-> arch.md missing; edges={:?}",
        graph.edges
    );

    let stub_names: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "import")
        .map(|n| n.name.as_str())
        .collect();
    assert!(
        stub_names.is_empty(),
        "every link in this corpus resolves; no bare-name import stub should remain, got {stub_names:?}"
    );

    // 4. Duplicate basename: deploy.md's `[home](./index.md)` must resolve to the
    // ROOT index.md (relative path resolution), never the `sub/index.md` sharing
    // its basename.
    assert!(
        has_edge(&graph, &deploy_m, &index_m, "imports"),
        "deploy.md -[inline]-> root index.md backlink missing; edges={:?}",
        graph.edges
    );
    assert!(
        !has_edge(&graph, &deploy_m, &sub_index_m, "imports"),
        "deploy.md's [home](./index.md) must not false-link the duplicate-basename sub/index.md"
    );

    // 5. Anchor / wikilink-with-anchor resolves to the target HEADING node, not
    // the module.
    let arch_overview = heading_id(&graph, "Overview", "arch.md");
    assert!(
        has_edge(&graph, &index_m, &arch_overview, "imports"),
        "index.md -[[arch#Overview]]-> arch.md#Overview heading missing; edges={:?}",
        graph.edges
    );
    assert!(
        !has_edge(&graph, &index_m, &arch_m, "imports"),
        "an anchored wikilink must not ALSO edge to arch.md's module"
    );
    assert!(
        has_edge(&graph, &guide_m, &arch_overview, "imports"),
        "guide.md -[Architecture Overview](./arch.md#overview)-> arch.md#Overview heading missing; edges={:?}",
        graph.edges
    );
    // Same-doc `[setup section](#setup)` resolves within guide.md itself, not
    // index.md's Setup heading (despite the surrounding prose saying "in the
    // index document" -- the link's own target is anchor-only).
    let guide_setup = heading_id(&graph, "Setup", "guide.md");
    assert!(
        has_edge(&graph, &guide_m, &guide_setup, "imports"),
        "guide.md's same-doc [setup section](#setup) must resolve to guide.md's own Setup heading; edges={:?}",
        graph.edges
    );
    assert!(
        !has_edge(&graph, &guide_m, &setup_id, "imports"),
        "guide.md's same-doc anchor must not cross-resolve to index.md's Setup heading"
    );

    // 6. Frontmatter: `title:` renames the module node; `tags:` mint shared
    // `kind:"tag"` nodes with `tagged` edges from guide.md.
    assert_eq!(
        graph.node(&guide_m).unwrap().name,
        "Implementation Guide",
        "guide.md's frontmatter title must rename its module node"
    );
    for tag in ["guide", "documentation", "reference"] {
        let t = tag_id(&graph, tag);
        assert!(
            has_edge(&graph, &guide_m, &t, "tagged"),
            "guide.md -> tag `{tag}` tagged edge missing; edges={:?}",
            graph.edges
        );
    }

    // Embed: notes.md's `![[deploy]]` -> an `embeds` edge to deploy.md's module.
    assert!(
        has_edge(&graph, &notes_m, &deploy_m, "embeds"),
        "notes.md -[![[deploy]]]-> deploy.md `embeds` edge missing; edges={:?}",
        graph.edges
    );

    // Inline `#tag`s: notes.md's #alpha/#beta/#documentation each reuse the same
    // shared `tag`-node path as frontmatter tags. `documentation` must be the SAME
    // node guide.md's frontmatter tags into (tag nodes are shared by name), not a
    // second one -- proving the shared tag-node model, not just parallel tagging.
    for tag in ["alpha", "beta", "documentation"] {
        let t = tag_id(&graph, tag);
        assert!(
            has_edge(&graph, &notes_m, &t, "tagged"),
            "notes.md -> tag `{tag}` tagged edge missing; edges={:?}",
            graph.edges
        );
    }
    let doc_tag_nodes: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "tag" && n.name == "documentation")
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(
        doc_tag_nodes.len(),
        1,
        "the `documentation` tag must be ONE shared node reached from both guide.md \
         (frontmatter) and notes.md (inline #tag), got {doc_tag_nodes:?}"
    );

    // 7. Regression gate: plain.md (pure CommonMark, no `[[`/`![[`/`#tag`)
    // contributes ZERO `tagged`/`embeds` edges. Its one inline link still
    // resolves normally (standard CommonMark stays owned by lens).
    let plain_ids: Vec<&str> = graph
        .nodes
        .iter()
        .filter(|n| n.file == "plain.md")
        .map(|n| n.id.as_str())
        .collect();
    assert!(
        !graph
            .edges
            .iter()
            .any(|e| plain_ids.contains(&e.from.as_str()) && (e.kind == "tagged" || e.kind == "embeds")),
        "plain.md must contribute zero tagged/embeds edges (PKM no-regression gate); edges={:?}",
        graph.edges
    );
}

/// guide.md's frontmatter declares `aliases: [impl-guide, how-to]`, but no link in
/// this corpus actually targets either alias, so the graph-over-the-fixture-corpus
/// test above can't observe alias resolution end-to-end. This exercises the SAME
/// declared alias values in an isolated fixture (mirroring the T5 alias-resolution
/// guarantee already unit-tested in `discovery::mod`) to prove the corpus's declared
/// aliases actually resolve rather than sitting unused.
#[test]
fn markdown_guide_aliases_resolve_when_linked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("guide.md"),
        "---\ntitle: Implementation Guide\naliases: [impl-guide, how-to]\n---\n# Guide\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("other.md"),
        "# Other\n\nSee [[impl-guide]] and [[how-to]].\n",
    )
    .unwrap();

    let graph = discovery::discover(dir.path(), None).unwrap().graph;
    let guide = module_id(&graph, "guide.md");
    let other = module_id(&graph, "other.md");

    assert!(
        has_edge(&graph, &other, &guide, "imports"),
        "[[impl-guide]] must resolve to guide.md via its declared alias; edges={:?}",
        graph.edges
    );
    assert!(
        !graph
            .nodes
            .iter()
            .any(|n| n.kind == "import" && (n.name == "impl-guide" || n.name == "how-to")),
        "declared aliases that resolve must not also leave unresolved-link stubs; nodes={:?}",
        graph.nodes
    );
}
