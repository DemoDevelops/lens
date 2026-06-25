//! Graph traversal: `lens_symbol`, `lens_links`, `lens_path`.

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
/// immediate (depth-1) connections as one combined subgraph. Matches are ranked
/// recently-touched-file first (session proximity, when `recent_files` is given),
/// then by structural importance (PageRank) so the central symbol outranks
/// same-substring decoys, so the most relevant matches survive the `limit` cut.
pub fn query(
    graph: &Graph,
    name: &str,
    kind: Option<&str>,
    limit: usize,
    recent_files: &[String],
) -> GraphView {
    let mut matches = graph.find_by_name(name, kind);
    // Rank: recent-file matches first (a no-op when `recent_files` is empty), then
    // by importance (descending), then by id for a stable, deterministic tie-break.
    let importance = graph.importance();
    matches.sort_by(|a, b| {
        let ra = is_recent(&a.file, recent_files);
        let rb = is_recent(&b.file, recent_files);
        rb.cmp(&ra)
            .then_with(|| {
                let ia = importance.get(&a.id).copied().unwrap_or(0.0);
                let ib = importance.get(&b.id).copied().unwrap_or(0.0);
                ib.partial_cmp(&ia).unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.id.cmp(&b.id))
    });
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

/// True when a node's (repo-relative) `file` corresponds to one of the session's
/// recently touched paths. Touched paths may be absolute or relative, so match on
/// a path-suffix relationship (one ends with the other on a `/` boundary) rather
/// than exact equality; a bare basename match is intentionally rejected so two
/// unrelated `mod.rs` files don't boost each other.
fn is_recent(node_file: &str, recent_files: &[String]) -> bool {
    let norm = |p: &str| p.replace('\\', "/");
    let node = norm(node_file);
    recent_files.iter().any(|r| {
        let r = norm(r);
        suffix_on_boundary(&r, &node) || suffix_on_boundary(&node, &r)
    })
}

/// True if `hay` ends with `needle` at a path-component boundary (i.e. the char
/// before the match is `/`, or `needle` is the whole string).
fn suffix_on_boundary(hay: &str, needle: &str) -> bool {
    if !hay.ends_with(needle) {
        return false;
    }
    let cut = hay.len() - needle.len();
    cut == 0 || hay.as_bytes()[cut - 1] == b'/'
}

/// Lexical natural-language find: tokenize `query` into words and rank symbol
/// names by lexical overlap (no embeddings). Per token, an exact name match beats
/// a prefix match beats a substring match, and a symbol gets a bonus for each
/// extra distinct query token it hits. Returns the top `limit` symbols plus their
/// immediate connections, like [`query`]. Case-insensitive.
pub fn find(graph: &Graph, query: &str, limit: usize) -> GraphView {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return subgraph(graph, &[]);
    }
    // Score every node; keep only those with a hit. Sort by score desc, then id
    // for a deterministic tie-break.
    let mut scored: Vec<(u32, &str)> = graph
        .nodes
        .iter()
        .filter_map(|n| {
            let s = score_name(&n.name, &tokens);
            (s > 0).then_some((s, n.id.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));

    let mut node_ids: Vec<String> = Vec::new();
    for (_, id) in scored.into_iter().take(limit) {
        node_ids.push(id.to_string());
        for e in graph.incident(id) {
            let other = if e.from == id { &e.to } else { &e.from };
            node_ids.push(other.clone());
        }
    }
    subgraph(graph, &node_ids)
}

/// Split a string into lowercase alphanumeric word tokens, also breaking
/// snake_case and camelCase so "build graph" hits `build_graph` / `buildGraph`.
fn tokenize(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    let flush = |cur: &mut String, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
    };
    for c in s.chars() {
        if c.is_alphanumeric() {
            // camelCase boundary: a lowercase/digit run followed by an uppercase.
            if c.is_uppercase() && prev_lower {
                flush(&mut cur, &mut out);
            }
            cur.extend(c.to_lowercase());
            prev_lower = c.is_lowercase() || c.is_numeric();
        } else {
            flush(&mut cur, &mut out);
            prev_lower = false;
        }
    }
    flush(&mut cur, &mut out);
    out
}

/// Lexical score of a symbol `name` against query `tokens`: exact (3) > prefix (2) >
/// substring (1) per token, summed over distinct hitting tokens, plus a bonus of
/// 2 per hit beyond the first so multi-token matches outrank single-token ones.
fn score_name(name: &str, tokens: &[String]) -> u32 {
    let lname = name.to_ascii_lowercase();
    let name_tokens = tokenize(name);
    let mut total = 0u32;
    let mut hits = 0u32;
    for t in tokens {
        let s = if name_tokens.iter().any(|nt| nt == t) {
            3
        } else if lname.starts_with(t.as_str()) {
            2
        } else if lname.contains(t.as_str()) {
            1
        } else {
            0
        };
        if s > 0 {
            total += s;
            hits += 1;
        }
    }
    if hits > 1 {
        total += 2 * (hits - 1);
    }
    total
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
    graph
        .find_by_name(token, None)
        .into_iter()
        .find_map(|n| {
            if n.name == token {
                Some(n.id.clone())
            } else {
                None
            }
        })
        // fall back to substring match
        .or_else(|| {
            graph
                .find_by_name(token, None)
                .first()
                .map(|n| n.id.clone())
        })
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

/// A token-budgeted overview of the repo's most important symbols (an aider-style
/// repomap): symbols ranked by structural importance, the largest prefix that fits
/// `token_budget` selected by binary search, each rendered with its kind, location,
/// top callers, and top callees. Gives an agent a high-signal map of a codebase at
/// a fixed token cost instead of reading files.
pub fn overview(graph: &Graph, token_budget: usize) -> String {
    let importance = graph.importance();
    let mut ranked: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| !matches!(n.kind.as_str(), "module" | "import"))
        .collect();
    ranked.sort_by(|a, b| {
        let ia = importance.get(&a.id).copied().unwrap_or(0.0);
        let ib = importance.get(&b.id).copied().unwrap_or(0.0);
        ib.partial_cmp(&ia)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    let n = ranked.len();
    let render = |k: usize| render_overview(graph, &ranked[..k]);
    // Largest prefix that fits the budget (binary search); if it all fits, all of it.
    if n == 0 || crate::obs::count_tokens(&render(n)) <= token_budget {
        return render(n);
    }
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if crate::obs::count_tokens(&render(mid)) <= token_budget {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    render(lo)
}

/// Render the given importance-ranked nodes as the overview body.
fn render_overview(graph: &Graph, nodes: &[&Node]) -> String {
    let name_of = |id: &str| graph.node(id).map(|n| n.name.clone());
    let mut s = String::from("# Repo overview: most important symbols\n\n");
    for n in nodes {
        s.push_str(&format!("- `{}` ({}) {}:{}\n", n.name, n.kind, n.file, n.line));
        let callers: Vec<String> = graph
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && e.to == n.id)
            .filter_map(|e| name_of(&e.from))
            .collect();
        if !callers.is_empty() {
            s.push_str(&format!("    called by: {}\n", join_capped(&callers, 3)));
        }
        let calls: Vec<String> = graph
            .edges
            .iter()
            .filter(|e| e.kind == "calls" && e.from == n.id)
            .filter_map(|e| name_of(&e.to))
            .collect();
        if !calls.is_empty() {
            s.push_str(&format!("    calls: {}\n", join_capped(&calls, 3)));
        }
    }
    s
}

/// Join up to `max` names, then "(+N more)".
fn join_capped(names: &[String], max: usize) -> String {
    let shown: Vec<&str> = names.iter().take(max).map(|s| s.as_str()).collect();
    let mut out = shown.join(", ");
    if names.len() > max {
        out.push_str(&format!(" (+{} more)", names.len() - max));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::discover;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn overview_budget_keeps_important_drops_rest() {
        // Two hubs called by many one-line workers. A tight budget must truncate to
        // the top-importance symbols, and the hubs must survive.
        let dir = tempdir().unwrap();
        let mut src = String::from("pub fn hub_x() -> i32 { 1 }\npub fn hub_y() -> i32 { 2 }\n");
        for i in 0..40 {
            src.push_str(&format!("pub fn w{i}() -> i32 {{ hub_x() + hub_y() }}\n"));
        }
        fs::write(dir.path().join("r.rs"), src).unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let full = overview(&g, 100_000);
        assert!(full.contains("`hub_x`") && full.contains("`w0`"), "full lists everything");

        let tight = overview(&g, 80);
        assert!(crate::obs::count_tokens(&tight) <= 80, "tight overview must fit the budget");
        // The hubs survive (entries carry backticks; a name in a caller list does not).
        assert!(
            tight.contains("`hub_x`") && tight.contains("`hub_y`"),
            "hubs survive truncation"
        );
        // Truncation happened: fewer symbol entries than the full overview.
        let entries = |s: &str| s.matches("- `").count();
        assert!(
            entries(&tight) < entries(&full),
            "tight overview drops low-importance symbols"
        );
    }

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
        let view = query(&g, "a", Some("function"), 20, &[]);
        assert!(view.nodes.iter().any(|n| n.name == "a"));
    }

    #[test]
    fn query_boosts_recently_touched_file() {
        // Two functions named so both match the substring "handler"; each lives in
        // a different file. With one of those files marked recently touched, its
        // symbol must sort first so a `limit` of 1 keeps it.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn alpha_handler() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "fn beta_handler() {}\n").unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        // Without proximity, id-sort order decides; assert the boost flips it.
        let recent = vec!["/abs/path/to/b.rs".to_string()];
        let view = query(&g, "handler", Some("function"), 1, &recent);
        assert!(
            view.nodes.iter().any(|n| n.name == "beta_handler"),
            "symbol from the recently touched file must rank first"
        );
        assert!(
            !view.nodes.iter().any(|n| n.name == "alpha_handler"),
            "the non-recent symbol must be cut by the limit"
        );
    }

    #[test]
    fn find_ranks_by_lexical_overlap() {
        // A natural-language query should surface the symbol whose name overlaps
        // most, even split across snake_case words.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("lib.rs"),
            "fn build_graph() {}\nfn parse_file() {}\nfn unrelated() {}\n",
        )
        .unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let view = find(&g, "build the graph please", 1);
        assert!(
            view.nodes.iter().any(|n| n.name == "build_graph"),
            "lexical find must return the best-matching symbol"
        );
        assert!(
            !view.nodes.iter().any(|n| n.name == "unrelated"),
            "a non-matching symbol must not be returned"
        );
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
