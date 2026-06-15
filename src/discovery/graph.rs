//! Structural graph data model: nodes (symbols) and edges (relationships),
//! with stable IDs and JSON (de)serialization.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// A symbol in the codebase: a function, method, type, module, or import target.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Node {
    /// Stable id: blake3(file:kind:name:line), hex-truncated.
    pub id: String,
    pub name: String,
    /// function | method | struct | enum | trait | interface | class | mod | module | import | type
    pub kind: String,
    pub file: String,
    /// 1-based line of the definition.
    pub line: usize,
    pub language: String,
}

impl Node {
    /// Compute the deterministic id for a symbol.
    pub fn make_id(file: &str, kind: &str, name: &str, line: usize) -> String {
        let key = format!("{file}:{kind}:{name}:{line}");
        blake3::hash(key.as_bytes()).to_hex()[..16].to_string()
    }

    pub fn new(file: &str, kind: &str, name: &str, line: usize, language: &str) -> Self {
        Node {
            id: Self::make_id(file, kind, name, line),
            name: name.to_string(),
            kind: kind.to_string(),
            file: file.to_string(),
            line,
            language: language.to_string(),
        }
    }
}

/// A directed relationship between two nodes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct Edge {
    pub from: String,
    pub to: String,
    /// calls | imports | contains | references
    pub kind: String,
}

/// The whole structural graph. Serializes to `.ctxforge/graph.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

impl Graph {
    pub fn new() -> Self {
        Graph::default()
    }

    /// Insert a node if its id is new; returns the id either way.
    pub fn add_node(&mut self, node: Node) -> String {
        let id = node.id.clone();
        if !self.nodes.iter().any(|n| n.id == id) {
            self.nodes.push(node);
        }
        id
    }

    /// Insert an edge if an identical one isn't already present.
    pub fn add_edge(&mut self, from: &str, to: &str, kind: &str) {
        let edge = Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
        };
        if !self.edges.contains(&edge) {
            self.edges.push(edge);
        }
    }

    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// All nodes whose name contains `needle` (case-insensitive), optionally
    /// filtered by kind. Results are sorted by id for determinism.
    pub fn find_by_name(&self, needle: &str, kind: Option<&str>) -> Vec<&Node> {
        let lneedle = needle.to_ascii_lowercase();
        let mut v: Vec<&Node> = self
            .nodes
            .iter()
            .filter(|n| n.name.to_ascii_lowercase().contains(&lneedle))
            .filter(|n| kind.map(|k| n.kind == k).unwrap_or(true))
            .collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Build a name -> node ids index (for resolving call/import targets).
    pub fn name_index(&self) -> HashMap<String, Vec<String>> {
        let mut idx: HashMap<String, Vec<String>> = HashMap::new();
        for n in &self.nodes {
            idx.entry(n.name.clone()).or_default().push(n.id.clone());
        }
        idx
    }

    /// Edges incident to `id` in either direction.
    pub fn incident(&self, id: &str) -> Vec<&Edge> {
        self.edges
            .iter()
            .filter(|e| e.from == id || e.to == id)
            .collect()
    }

    /// Undirected adjacency over edges whose kind passes `keep`.
    fn adjacency(&self, keep: impl Fn(&str) -> bool) -> HashMap<String, Vec<String>> {
        let mut adj: HashMap<String, Vec<String>> = HashMap::new();
        for e in &self.edges {
            if !keep(&e.kind) {
                continue;
            }
            adj.entry(e.from.clone()).or_default().push(e.to.clone());
            adj.entry(e.to.clone()).or_default().push(e.from.clone());
        }
        adj
    }

    /// Subgraph within `depth` hops of `start` (undirected BFS). Returns the
    /// reachable nodes and the edges among them.
    pub fn neighbors(&self, start: &str, depth: usize) -> (Vec<Node>, Vec<Edge>) {
        // Neighbors include every relationship, hierarchy included.
        let adj = self.adjacency(|_| true);
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(start.to_string());
        let mut frontier = vec![start.to_string()];
        for _ in 0..depth {
            let mut next = Vec::new();
            for id in &frontier {
                if let Some(neigh) = adj.get(id) {
                    for n in neigh {
                        if visited.insert(n.clone()) {
                            next.push(n.clone());
                        }
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        let nodes: Vec<Node> = self
            .nodes
            .iter()
            .filter(|n| visited.contains(&n.id))
            .cloned()
            .collect();
        let edges: Vec<Edge> = self
            .edges
            .iter()
            .filter(|e| visited.contains(&e.from) && visited.contains(&e.to))
            .cloned()
            .collect();
        (nodes, edges)
    }

    /// Shortest path (node ids) between two nodes via undirected BFS.
    /// Returns `None` if disconnected.
    pub fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        if from == to {
            return Some(vec![from.to_string()]);
        }
        // Reachability follows semantic flow (calls/imports), not containment,
        // so two symbols in the same file aren't trivially "connected".
        let adj = self.adjacency(|kind| kind != "contains");
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(from.to_string());
        let mut q = VecDeque::new();
        q.push_back(from.to_string());
        while let Some(cur) = q.pop_front() {
            if let Some(neigh) = adj.get(&cur) {
                // Deterministic order.
                let mut neigh = neigh.clone();
                neigh.sort();
                for n in neigh {
                    if visited.insert(n.clone()) {
                        prev.insert(n.clone(), cur.clone());
                        if n == to {
                            // reconstruct
                            let mut path = vec![to.to_string()];
                            let mut step = to.to_string();
                            while let Some(p) = prev.get(&step) {
                                path.push(p.clone());
                                step = p.clone();
                            }
                            path.reverse();
                            return Some(path);
                        }
                        q.push_back(n);
                    }
                }
            }
        }
        None
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json).with_context(|| format!("writing graph {}", path.display()))?;
        Ok(())
    }

    pub fn load(path: &Path) -> Result<Graph> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("reading graph {}", path.display()))?;
        let g: Graph = serde_json::from_str(&data)?;
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Graph {
        let mut g = Graph::new();
        let a = g.add_node(Node::new("f.rs", "function", "a", 1, "rust"));
        let b = g.add_node(Node::new("f.rs", "function", "b", 5, "rust"));
        let c = g.add_node(Node::new("f.rs", "function", "c", 9, "rust"));
        let d = g.add_node(Node::new("f.rs", "function", "d", 13, "rust"));
        g.add_edge(&a, &b, "calls");
        g.add_edge(&b, &c, "calls");
        // d is disconnected
        let _ = d;
        g
    }

    #[test]
    fn stable_ids() {
        let id1 = Node::make_id("f.rs", "function", "a", 1);
        let id2 = Node::make_id("f.rs", "function", "a", 1);
        assert_eq!(id1, id2);
        assert_ne!(id1, Node::make_id("f.rs", "function", "a", 2));
    }

    #[test]
    fn add_node_dedups() {
        let mut g = Graph::new();
        g.add_node(Node::new("f.rs", "function", "a", 1, "rust"));
        g.add_node(Node::new("f.rs", "function", "a", 1, "rust"));
        assert_eq!(g.nodes.len(), 1);
    }

    #[test]
    fn path_between_connected() {
        let g = sample();
        let a = Node::make_id("f.rs", "function", "a", 1);
        let c = Node::make_id("f.rs", "function", "c", 9);
        let path = g.shortest_path(&a, &c).unwrap();
        assert_eq!(path.len(), 3); // a -> b -> c
        assert_eq!(path[0], a);
        assert_eq!(path[2], c);
    }

    #[test]
    fn no_path_between_disconnected() {
        let g = sample();
        let a = Node::make_id("f.rs", "function", "a", 1);
        let d = Node::make_id("f.rs", "function", "d", 13);
        assert!(g.shortest_path(&a, &d).is_none());
    }

    #[test]
    fn neighbors_depth() {
        let g = sample();
        let a = Node::make_id("f.rs", "function", "a", 1);
        let (n1, _) = g.neighbors(&a, 1);
        assert_eq!(n1.len(), 2); // a, b
        let (n2, _) = g.neighbors(&a, 2);
        assert_eq!(n2.len(), 3); // a, b, c
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("graph.json");
        let g = sample();
        g.save(&p).unwrap();
        let g2 = Graph::load(&p).unwrap();
        assert_eq!(g.nodes.len(), g2.nodes.len());
        assert_eq!(g.edges.len(), g2.edges.len());
    }
}
