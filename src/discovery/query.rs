//! Graph traversal: `lens_symbol`, `lens_links`, `lens_path`.

use std::collections::HashMap;

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
///
/// Ranking is [`FindRank::Blend`]: lexical score stays the primary key (an exact
/// match is never demoted), and query-seeded personalized PageRank breaks ties
/// toward the canonically-referenced definition — the only thing that changes
/// vs raw lexical is which of several equal-score collisions (the many `index` /
/// `render` / same-named components a frontend has) survives the budget cut.
pub fn find(graph: &Graph, query: &str, limit: usize) -> GraphView {
    find_ranked(graph, query, limit, FindRank::Blend)
}

/// How [`find_ranked`] orders the lexical candidate set before the `limit` cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindRank {
    /// Pure lexical: score (desc), then id. The pre-L36 ranking, kept as the
    /// A/B control and the no-tie fast path inside [`FindRank::Blend`].
    Raw,
    /// Query-seeded personalized PageRank (L36) as the PRIMARY key, lexical score
    /// then id as tie-breaks. Surfaces a low-lexical-but-central hop into the
    /// budgeted top-`limit` when the query's matches transitively reach it.
    Personalized,
    /// Lexical score PRIMARY, personalized PR only as the tie-break. Preserves
    /// top-rank lexical fidelity (no MRR dip) but, being lexical-first, cannot
    /// lift a low-lexical hub past higher-lexical decoys into a small budget.
    Blend,
}

/// Lexical candidate find re-ranked by `rank`, returning the top `limit` symbols
/// plus their immediate connections. The candidate SET is identical across
/// rankings (every symbol with a lexical hit); only the order — and thus which
/// survive the `limit` cut and contribute their neighbors — changes.
pub fn find_ranked(graph: &Graph, query: &str, limit: usize, rank: FindRank) -> GraphView {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return subgraph(graph, &[]);
    }
    // Score every node; keep only those with a hit.
    let mut scored: Vec<(u32, &str)> = graph
        .nodes
        .iter()
        .filter_map(|n| {
            let s = score_name(&n.name, &tokens);
            (s > 0).then_some((s, n.id.as_str()))
        })
        .collect();
    // Raw lexical order: score desc, then id. The starting point for every rank.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    match rank {
        FindRank::Raw => {}
        FindRank::Personalized => {
            // Seed personalized PR with the per-candidate lexical scores, then
            // order by PR PRIMARY (lexical score, id as tie-breaks).
            let seed: HashMap<String, f64> =
                scored.iter().map(|(s, id)| (id.to_string(), *s as f64)).collect();
            let pr = graph.personalized_importance(&seed);
            scored.sort_by(|a, b| {
                pr_cmp(&pr, a, b)
                    .then_with(|| b.0.cmp(&a.0))
                    .then_with(|| a.1.cmp(b.1))
            });
        }
        FindRank::Blend => {
            // Lexical PRIMARY with personalized PR only as the tie-break. The PR
            // can change the SELECTED top-`limit` set only when a lexical-score
            // tie straddles the `limit` cut (equal-score members on both sides of
            // the boundary). When the cut is clean — the common case, including
            // every unique-name query — the set is identical to Raw, so we skip
            // the power iteration entirely and Blend is a provable no-op.
            let straddles =
                limit > 0 && scored.len() > limit && scored[limit - 1].0 == scored[limit].0;
            if straddles {
                let seed: HashMap<String, f64> =
                    scored.iter().map(|(s, id)| (id.to_string(), *s as f64)).collect();
                let pr = graph.personalized_importance(&seed);
                // Re-sort lexical-primary, PR then id: only reorders within
                // equal-score groups, pulling the canonical (highest-PR) member of
                // the straddling tie across the cut.
                scored.sort_by(|a, b| {
                    b.0.cmp(&a.0)
                        .then_with(|| pr_cmp(&pr, a, b))
                        .then_with(|| a.1.cmp(b.1))
                });
            }
        }
    }

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

/// Compare two scored candidates by descending personalized-PR weight (the
/// shared tie-break/primary used by [`FindRank::Personalized`] and
/// [`FindRank::Blend`]). Missing weights sort as 0.
fn pr_cmp(pr: &HashMap<String, f64>, a: &(u32, &str), b: &(u32, &str)) -> std::cmp::Ordering {
    let pa = pr.get(a.1).copied().unwrap_or(0.0);
    let pb = pr.get(b.1).copied().unwrap_or(0.0);
    pb.partial_cmp(&pa).unwrap_or(std::cmp::Ordering::Equal)
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

/// Header every overview starts with.
const OVERVIEW_HEADER: &str = "# Repo overview: most important symbols\n\n";

/// Upper bound on the knapsack DP table size (`n * capacity` cells, each an `f64`);
/// above it we fall back to the cheaper prefix heuristic to cap memory (~64 MB).
const KNAPSACK_MAX_CELLS: usize = 8_000_000;

/// A token-budgeted overview of the repo's most important symbols (an aider-style
/// repomap): symbols ranked by structural importance, then the value(importance)-
/// maximising subset that fits `token_budget` chosen by a 0/1 knapsack over each
/// node's render-token weight, each rendered with its kind, location, top callers,
/// and top callees. Skipping a token-heavy hub to pack many cheaper important ones
/// keeps more important-symbol mass than the largest fitting prefix would. Falls
/// back to a binary-searched prefix when the DP table would be too large. Gives an
/// agent a high-signal map of a codebase at a fixed token cost instead of reading
/// files.
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
    if n == 0 {
        return render_overview(graph, &[]);
    }
    // Everything fits: emit the whole map in importance order.
    let full = render_overview(graph, &ranked);
    if crate::obs::count_tokens(&full) <= token_budget {
        return full;
    }
    // Budget the per-node entries against what's left after the header. Token count
    // is subadditive (`count_tokens(a ++ b) <= count_tokens(a) + count_tokens(b)`),
    // so a subset whose per-entry weights sum within `capacity` is guaranteed to
    // render within `token_budget` once concatenated with the header.
    let capacity = token_budget.saturating_sub(crate::obs::count_tokens(OVERVIEW_HEADER));
    // Memory guard: very large graphs fall back to the prefix heuristic.
    if n.saturating_mul(capacity) > KNAPSACK_MAX_CELLS {
        return fit_prefix(graph, &ranked, token_budget);
    }
    let weights: Vec<usize> = ranked
        .iter()
        .map(|node| crate::obs::count_tokens(&render_entry(graph, node)))
        .collect();
    let values: Vec<f64> = ranked
        .iter()
        .map(|node| importance.get(&node.id).copied().unwrap_or(0.0))
        .collect();
    let picked: Vec<&Node> = knapsack(&weights, &values, capacity)
        .into_iter()
        .map(|i| ranked[i])
        .collect();
    render_overview(graph, &picked)
}

/// Largest importance-ranked PREFIX whose render fits `token_budget` (binary
/// search). The knapsack memory fallback.
fn fit_prefix(graph: &Graph, ranked: &[&Node], token_budget: usize) -> String {
    let render = |k: usize| render_overview(graph, &ranked[..k]);
    let mut lo = 0usize;
    let mut hi = ranked.len();
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

/// 0/1 knapsack: pick the `values`-maximising subset of items whose `weights` sum
/// is within `capacity`. Returns the chosen indices in ascending order (preserving
/// the input's importance ranking). Classic `(n+1) x (capacity+1)` DP table.
fn knapsack(weights: &[usize], values: &[f64], capacity: usize) -> Vec<usize> {
    let n = weights.len();
    let mut dp = vec![vec![0f64; capacity + 1]; n + 1];
    for i in 1..=n {
        let (wi, vi) = (weights[i - 1], values[i - 1]);
        for w in 0..=capacity {
            let without = dp[i - 1][w];
            dp[i][w] = if wi <= w {
                without.max(dp[i - 1][w - wi] + vi)
            } else {
                without
            };
        }
    }
    // Reconstruct: item `i` was taken iff including it reproduces this cell's value.
    // The comparison is bit-exact because the cell was assigned that same float
    // expression when the `max` chose the "with" branch.
    let mut chosen = Vec::new();
    let mut w = capacity;
    for i in (1..=n).rev() {
        let (wi, vi) = (weights[i - 1], values[i - 1]);
        if wi <= w && dp[i][w] == dp[i - 1][w - wi] + vi {
            chosen.push(i - 1);
            w -= wi;
        }
    }
    chosen.reverse();
    chosen
}

/// Render the given importance-ranked nodes as the overview body.
fn render_overview(graph: &Graph, nodes: &[&Node]) -> String {
    let mut s = String::from(OVERVIEW_HEADER);
    for n in nodes {
        s.push_str(&render_entry(graph, n));
    }
    s
}

/// Render one node's overview entry: its markdown line plus capped caller/callee
/// lines. This is the unit the knapsack weighs.
fn render_entry(graph: &Graph, n: &Node) -> String {
    let name_of = |id: &str| graph.node(id).map(|n| n.name.clone());
    let mut s = format!("- `{}` ({}) {}:{}\n", n.name, n.kind, n.file, n.line);
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

    /// Five symbols all match "order" with the SAME lexical score (a tie). Four
    /// `order_zint_*`/`order_zsink` form a cluster where the callers invoke the
    /// `order_zsink` hub; `order_aaa_leaf` is disconnected. Raw lexical breaks the
    /// tie arbitrarily (by id hash); the shipped default (Blend) breaks it by
    /// personalized PR toward `order_zsink`, the definition the other matches
    /// call. The frontend collision case in miniature.
    fn order_tie_graph() -> Graph {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("r.rs"),
            "fn order_aaa_leaf() {}\n\
             fn order_zsink() {}\n\
             fn order_zint_1() { order_zsink(); }\n\
             fn order_zint_2() { order_zsink(); }\n\
             fn order_zint_3() { order_zsink(); }\n",
        )
        .unwrap();
        discover(dir.path(), None).unwrap().graph
    }

    #[test]
    fn find_defaults_to_blend_tiebreak_pulls_central_into_budget() {
        let g = order_tie_graph();
        let names = |v: &GraphView| {
            v.nodes
                .iter()
                .map(|n| n.name.clone())
                .collect::<std::collections::HashSet<_>>()
        };
        // Blend selects the called-by-all hub as the single primary, pulling its
        // whole caller cluster into the 1-slot budget.
        let blend = names(&find(&g, "order", 1));
        for c in ["order_zsink", "order_zint_1", "order_zint_2", "order_zint_3"] {
            assert!(blend.contains(c), "blend should surface the hub cluster member {c}");
        }
        // Raw, lacking the PR tie-break, picks an arbitrary tied match -> a
        // different, smaller selected set.
        let raw = names(&find_ranked(&g, "order", 1, FindRank::Raw));
        assert_ne!(blend, raw, "the PR tie-break must change the selected set vs raw");
    }

    #[test]
    fn find_blend_is_noop_when_cut_is_clean() {
        // Budget covers every match -> no lexical tie straddles the cut, so Blend
        // must skip the power iteration and return exactly the Raw node set.
        let g = order_tie_graph();
        let ids = |v: &GraphView| {
            let mut out: Vec<String> = v.nodes.iter().map(|n| n.id.clone()).collect();
            out.sort();
            out
        };
        assert_eq!(
            ids(&find(&g, "order", 10)),
            ids(&find_ranked(&g, "order", 10, FindRank::Raw)),
            "blend is a no-op vs raw when the budget cut is clean"
        );
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
