//! Graph traversal: `graph_query`, `graph_neighbors`, `graph_path`.

use super::graph::{Edge, Graph, Node};
use crate::tools::{EdgeView, GraphView, NodeView, PathResponse};

fn node_view(n: &Node) -> NodeView {
    NodeView {
        id: n.id.clone(),
        name: n.name.clone(),
        kind: n.kind.clone(),
        file: n.file.clone(),
        line: n.line,
        language: n.language.clone(),
    }
}

fn edge_view(e: &Edge) -> EdgeView {
    EdgeView {
        from: e.from.clone(),
        to: e.to.clone(),
        kind: e.kind.clone(),
    }
}

/// Find nodes by name substring (+ optional kind), returning each match plus its
/// immediate (depth-1) connections as one combined subgraph.
pub fn query(graph: &Graph, name: &str, kind: Option<&str>, limit: usize) -> GraphView {
    let matches = graph.find_by_name(name, kind);
    let mut node_ids: Vec<String> = Vec::new();
    for m in matches.into_iter().take(limit) {
        node_ids.push(m.id.clone());
        // immediate neighbors
        for e in graph.incident(&m.id) {
            let other = if e.from == m.id { &e.to } else { &e.from };
            node_ids.push(other.clone());
        }
    }
    subgraph(graph, &node_ids)
}

/// Local subgraph within `depth` hops of `node_id`.
pub fn neighbors(graph: &Graph, node_id: &str, depth: usize) -> GraphView {
    let (nodes, edges) = graph.neighbors(node_id, depth);
    GraphView {
        nodes: nodes.iter().map(node_view).collect(),
        edges: edges.iter().map(edge_view).collect(),
        compact: None,
        truncated: false,
        retrieve_ref: None,
    }
}

/// Shortest path between two symbols (by id or name).
pub fn path(graph: &Graph, from: &str, to: &str) -> PathResponse {
    let from_id = resolve(graph, from);
    let to_id = resolve(graph, to);
    let (from_id, to_id) = match (from_id, to_id) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return PathResponse {
                found: false,
                path: vec![],
            }
        }
    };
    match graph.shortest_path(&from_id, &to_id) {
        Some(ids) => {
            let path: Vec<NodeView> = ids
                .iter()
                .filter_map(|id| graph.node(id))
                .map(node_view)
                .collect();
            PathResponse { found: true, path }
        }
        None => PathResponse {
            found: false,
            path: vec![],
        },
    }
}

/// Resolve a token to a node id: exact id match first, then first name match.
fn resolve(graph: &Graph, token: &str) -> Option<String> {
    if graph.node(token).is_some() {
        return Some(token.to_string());
    }
    graph.find_by_name(token, None).into_iter().find_map(|n| {
        if n.name == token {
            Some(n.id.clone())
        } else {
            None
        }
    })
    // fall back to substring match
    .or_else(|| graph.find_by_name(token, None).first().map(|n| n.id.clone()))
}

/// Build a deduplicated subgraph from a set of node ids, including edges whose
/// endpoints are both in the set.
fn subgraph(graph: &Graph, ids: &[String]) -> GraphView {
    let mut seen: Vec<String> = Vec::new();
    for id in ids {
        if !seen.contains(id) {
            seen.push(id.clone());
        }
    }
    let nodes: Vec<NodeView> = seen
        .iter()
        .filter_map(|id| graph.node(id))
        .map(node_view)
        .collect();
    let edges: Vec<EdgeView> = graph
        .edges
        .iter()
        .filter(|e| seen.contains(&e.from) && seen.contains(&e.to))
        .map(edge_view)
        .collect();
    GraphView {
        nodes,
        edges,
        compact: None,
        truncated: false,
        retrieve_ref: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::discover;
    use std::fs;
    use tempfile::tempdir;

    fn rust_graph() -> Graph {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\nfn lonely() {}\n",
        )
        .unwrap();
        discover(dir.path(), None).unwrap().graph
    }

    #[test]
    fn query_finds_known_function() {
        let g = rust_graph();
        let view = query(&g, "a", Some("function"), 20);
        assert!(view.nodes.iter().any(|n| n.name == "a"));
    }

    #[test]
    fn path_between_connected_symbols() {
        let g = rust_graph();
        let resp = path(&g, "a", "c");
        assert!(resp.found);
        assert!(resp.path.len() >= 2);
        assert_eq!(resp.path.first().unwrap().name, "a");
        assert_eq!(resp.path.last().unwrap().name, "c");
    }

    #[test]
    fn no_path_between_disconnected_symbols() {
        let g = rust_graph();
        let resp = path(&g, "a", "lonely");
        assert!(!resp.found);
        assert!(resp.path.is_empty());
    }

    #[test]
    fn neighbors_respects_depth() {
        let g = rust_graph();
        let a_id = g
            .find_by_name("a", Some("function"))
            .first()
            .unwrap()
            .id
            .clone();
        let d1 = neighbors(&g, &a_id, 1);
        let d2 = neighbors(&g, &a_id, 2);
        assert!(d2.nodes.len() >= d1.nodes.len());
    }
}
