//! Graph traversal: `lens_symbol`, `lens_links`, `lens_path`.

use std::collections::HashMap;

use super::graph::{Direction, Edge, Graph, Node};
use crate::tools::{EdgeView, GraphView, NodeView, PathResponse, ResolvedNote};

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
    let total = matches.len();
    let mut node_ids: Vec<String> = Vec::new();
    for m in matches.into_iter().take(limit) {
        node_ids.push(m.id.clone());
        // immediate neighbors
        for e in graph.incident(&m.id) {
            let other = if e.from == m.id { &e.to } else { &e.from };
            node_ids.push(other.clone());
        }
    }
    let mut view = subgraph(graph, &node_ids);
    view.total_matches = (total > limit).then_some(total);
    view.resolved = ambiguity_note(graph, name);
    view
}

/// The ambiguity note [`query`]/[`find_ranked_filtered`] attach to their
/// [`GraphView`]: reuses [`resolve_note`]'s exact-name ranking (the same
/// machinery `path` surfaces via `PathResponse::resolved`) so a query that
/// exactly names more than one symbol reports which one won and how many it
/// beat. Empty when unambiguous (or when nothing matches by exact name).
fn ambiguity_note(graph: &Graph, token: &str) -> Vec<ResolvedNote> {
    match resolve_note(graph, token) {
        Some((chosen, other_candidates)) if other_candidates > 0 => vec![ResolvedNote {
            query: token.to_string(),
            chosen,
            other_candidates,
        }],
        _ => Vec::new(),
    }
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

/// Like [`find`] but restricting the lexical candidate set to nodes of `kind`
/// (function | struct | class | method | ...) before ranking, so a wrong-kind
/// same-name symbol can't survive the budget. `None` = no filter (identical to
/// [`find`]). The `kind`-threaded seam consumed by `lens_find`.
pub fn find_kind(graph: &Graph, query: &str, limit: usize, kind: Option<&str>) -> GraphView {
    find_ranked_filtered(graph, query, limit, kind, FindRank::Blend)
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
    find_ranked_filtered(graph, query, limit, None, rank)
}

/// [`find_ranked`] with an optional `kind` filter applied to the candidate set
/// before scoring/ranking. Split out so `lens_find`'s kind filter and the
/// existing rank-only entry point share one body. `kind == None` is identical to
/// [`find_ranked`].
fn find_ranked_filtered(
    graph: &Graph,
    query: &str,
    limit: usize,
    kind: Option<&str>,
    rank: FindRank,
) -> GraphView {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return subgraph(graph, &[]);
    }
    // Score every node of the requested kind; keep only those with a hit.
    let mut scored: Vec<(u32, &str)> = graph
        .nodes
        .iter()
        .filter(|n| kind.map(|k| n.kind == k).unwrap_or(true))
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

    let total = scored.len();
    let mut node_ids: Vec<String> = Vec::new();
    for (_, id) in scored.into_iter().take(limit) {
        node_ids.push(id.to_string());
        for e in graph.incident(id) {
            let other = if e.from == id { &e.to } else { &e.from };
            node_ids.push(other.clone());
        }
    }
    let mut view = subgraph(graph, &node_ids);
    view.total_matches = (total > limit).then_some(total);
    view.resolved = ambiguity_note(graph, query);
    view
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
/// sub-token-boundary (1) per token, summed over distinct hitting tokens, plus a
/// bonus of 2 per hit beyond the first so multi-token matches outrank single-token
/// ones.
///
/// The tier-1 hit is boundary-anchored, not a raw substring: `name` is split on
/// `_` and case transitions ([`tokenize`]), and a query token scores 1 only when
/// it is the PREFIX of one of those sub-tokens (i.e. it lands on a `_`/camelCase
/// boundary). So `"get"` matches `get_config`/`target_getter` but no longer
/// matches `Widget` (mid-token) or `CONTROL_BUDGET` (neither `control` nor
/// `budget` starts with `get`), the measured false-positive.
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
        } else if name_tokens.iter().any(|nt| nt.starts_with(t.as_str())) {
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

/// Local subgraph within `depth` hops of `node_id` (undirected: every relation,
/// both senses). Equivalent to [`neighbors_dir`] with `"both"`.
pub fn neighbors(graph: &Graph, node_id: &str, depth: usize) -> GraphView {
    neighbors_dir(graph, node_id, depth, None)
}

/// Local subgraph within `depth` hops of `node_id`, walking edges in `dir`:
/// `"callers"` (fan-in, reverse edges), `"callees"` (fan-out, forward edges), or
/// `"both"` / unknown / `None` (undirected, byte-for-byte the legacy
/// [`neighbors`]). Threads T2's [`Direction`] into `lens_links`.
pub fn neighbors_dir(graph: &Graph, node_id: &str, depth: usize, dir: Option<&str>) -> GraphView {
    let direction = match dir {
        Some("callers") => Direction::Callers,
        Some("callees") => Direction::Callees,
        _ => Direction::Both,
    };
    let (nodes, edges) = graph.neighbors_directed(node_id, depth, direction);
    GraphView {
        nodes: nodes.iter().map(node_view).collect(),
        edges: edges.iter().map(edge_view).collect(),
        compact: None,
        truncated: false,
        retrieve_ref: None,
        resolved: Vec::new(),
        total_matches: None,
        trim_note: None,
    }
}

/// Shortest path between two symbols (by id or name). Carries per-hop edge kinds
/// (`edges`) and, when a `from`/`to` name was ambiguous, resolution notes naming
/// the chosen node and how many same-name candidates it beat (`resolved`).
pub fn path(graph: &Graph, from: &str, to: &str) -> PathResponse {
    let from_r = resolve_note(graph, from);
    let to_r = resolve_note(graph, to);
    // Surface ambiguity: a note per end that had >1 exact-name candidate.
    let mut resolved: Vec<ResolvedNote> = Vec::new();
    for (token, r) in [(from, &from_r), (to, &to_r)] {
        if let Some((id, others)) = r {
            if *others > 0 {
                resolved.push(ResolvedNote {
                    query: token.to_string(),
                    chosen: id.clone(),
                    other_candidates: *others,
                });
            }
        }
    }
    let (from_id, to_id) = match (&from_r, &to_r) {
        (Some((a, _)), Some((b, _))) => (a.clone(), b.clone()),
        _ => {
            return PathResponse {
                found: false,
                path: vec![],
                edges: vec![],
                resolved,
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
            let edges = path_edges(graph, &ids);
            PathResponse {
                found: true,
                path,
                edges,
                resolved,
            }
        }
        None => PathResponse {
            found: false,
            path: vec![],
            edges: vec![],
            resolved,
        },
    }
}

/// Per-hop edges aligned with a node-id path: for each consecutive `(u, v)`, the
/// connecting edge (excluding `contains`, matching [`Graph::shortest_path`]'s
/// traversal), preferring the forward `u -> v` orientation but falling back to a
/// reverse `v -> u` edge (undirected mode). Returned [`EdgeView`]s keep their real
/// `from`/`to`/`kind`. Length is `ids.len().saturating_sub(1)`.
fn path_edges(graph: &Graph, ids: &[String]) -> Vec<EdgeView> {
    ids.windows(2)
        .map(|w| {
            let (u, v) = (&w[0], &w[1]);
            graph
                .edges
                .iter()
                .find(|e| e.kind != "contains" && e.from == *u && e.to == *v)
                .or_else(|| {
                    graph
                        .edges
                        .iter()
                        .find(|e| e.kind != "contains" && e.from == *v && e.to == *u)
                })
                .map(edge_view)
                // Defensive: a real path hop always has a connecting edge; keep the
                // alignment invariant if one is somehow absent.
                .unwrap_or_else(|| EdgeView {
                    from: u.clone(),
                    to: v.clone(),
                    kind: "unknown".to_string(),
                })
        })
        .collect()
}

/// Resolve a token (node id or symbol name) to a node id, picking the highest
/// graph-importance candidate among same-name matches. The shared resolver for
/// `lens_path` and (T8) `lens_links` name inputs, so both resolve identically.
pub fn resolve(graph: &Graph, token: &str) -> Option<String> {
    resolve_note(graph, token).map(|(id, _)| id)
}

/// Resolve `token` to a node id plus the count of OTHER exact-name candidates it
/// beat. An exact id match wins outright (0 others). Otherwise, among exact-NAME
/// matches, the highest graph-[`importance`](Graph::importance) node is chosen
/// (id tie-break) — replacing the old id-hash-order pick that made
/// `lens_path(from="main")` resolve to a `benchmarks/` fixture instead of the
/// real entry point. Falls back to the first substring match (id order, reported
/// unambiguous) when nothing matches by exact name.
fn resolve_note(graph: &Graph, token: &str) -> Option<(String, usize)> {
    if graph.node(token).is_some() {
        return Some((token.to_string(), 0));
    }
    let by_name = graph.find_by_name(token, None);
    let mut exact: Vec<&Node> = by_name.iter().copied().filter(|n| n.name == token).collect();
    if !exact.is_empty() {
        // T7 seam: once nodes carry a prod/test/bench origin flag, importance()
        // discounts test/bench nodes, so this same ranking prefers the prod
        // definition for free — no change needed here.
        let importance = graph.importance();
        exact.sort_by(|a, b| {
            let ia = importance.get(&a.id).copied().unwrap_or(0.0);
            let ib = importance.get(&b.id).copied().unwrap_or(0.0);
            ib.partial_cmp(&ia)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        return Some((exact[0].id.clone(), exact.len() - 1));
    }
    // Substring fallback: preserve the legacy first-by-id pick (find_by_name is
    // id-sorted), reported as unambiguous.
    by_name.first().map(|n| (n.id.clone(), 0))
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
        resolved: Vec::new(),
        total_matches: None,
        trim_note: None,
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
///
/// `seed` personalizes the ranking (L40): an aider-style focus vector over node
/// ids (session-touched files and `query`-matched names, built by
/// [`overview_seed`]) fed to [`Graph::personalized_importance`]. An empty seed
/// reduces to the global PageRank teleport by construction, so a no-focus overview
/// is byte-identical to the static map: one code path, never a branch on emptiness.
pub fn overview(graph: &Graph, token_budget: usize, seed: &HashMap<String, f64>) -> String {
    overview_ranked(graph, token_budget, &graph.personalized_importance(seed))
}

/// The ordering and knapsack-packing core of [`overview`], rendering from a
/// precomputed `importance` map. Split out so the exact ranking is injectable:
/// [`overview`] passes seeded PageRank; the regression test passes
/// `graph.importance()` directly to pin empty-seed == global-PR byte-for-byte.
fn overview_ranked(
    graph: &Graph,
    token_budget: usize,
    importance: &HashMap<String, f64>,
) -> String {
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

/// Build the [`overview`] personalization seed (aider-style repomap focus): each
/// node gets +50 when its file was touched this session (`recent_files`) and +10
/// when its name matches the optional `query`, so a node hit by both weighs 60.
/// Reuses the same session-proximity ([`is_recent`]) and lexical ([`score_name`])
/// signals as `lens_symbol` and `lens_find`. An empty result (no touched files and
/// no query match) leaves [`overview`] byte-identical to the global map.
pub fn overview_seed(
    graph: &Graph,
    recent_files: &[String],
    query: Option<&str>,
) -> HashMap<String, f64> {
    let tokens = query.map(tokenize);
    let mut seed: HashMap<String, f64> = HashMap::new();
    for node in &graph.nodes {
        let mut w = 0.0;
        if is_recent(&node.file, recent_files) {
            w += 50.0;
        }
        if tokens.as_ref().is_some_and(|t| score_name(&node.name, t) > 0) {
            w += 10.0;
        }
        if w > 0.0 {
            *seed.entry(node.id.clone()).or_insert(0.0) += w;
        }
    }
    seed
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

        let full = overview(&g, 100_000, &HashMap::new());
        assert!(full.contains("`hub_x`") && full.contains("`w0`"), "full lists everything");

        let tight = overview(&g, 80, &HashMap::new());
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

    #[test]
    fn overview_empty_seed_equals_global_pr_render() {
        // The byte-identity pin. An empty seed MUST flow through
        // personalized_importance(&empty), which reduces to importance()'s uniform
        // teleport. Rendering directly from graph.importance() via the shared
        // packing core (overview_ranked) must be byte-for-byte identical to
        // overview() with an empty seed, at both a generous and a truncating budget
        // (covers ordering AND packing). A future second code path for the empty
        // case would break this.
        let dir = tempdir().unwrap();
        let mut src = String::from("pub fn hub_x() -> i32 { 1 }\npub fn hub_y() -> i32 { 2 }\n");
        for i in 0..40 {
            src.push_str(&format!("pub fn w{i}() -> i32 {{ hub_x() + hub_y() }}\n"));
        }
        fs::write(dir.path().join("r.rs"), src).unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let empty = HashMap::new();
        for budget in [100_000usize, 80] {
            assert_eq!(
                overview(&g, budget, &empty),
                overview_ranked(&g, budget, &g.importance()),
                "empty-seed overview must equal the global-PR render byte-for-byte (budget {budget})"
            );
        }
    }

    #[test]
    fn overview_focus_lifts_touched_file_into_budget() {
        // A cluster of hubs (each called by 40 workers) dominates global
        // importance; an isolated helper in its own file is globally unimportant. A
        // budget sized to exactly the two hubs excludes the helper. Marking the
        // helper's file touched seeds its node (+50), so personalized PageRank lifts
        // it into the same budget, proving personalization changes the packed set
        // while the empty path stays fixed.
        let dir = tempdir().unwrap();
        let mut core = String::from("pub fn hub_x() -> i32 { 1 }\npub fn hub_y() -> i32 { 2 }\n");
        for i in 0..40 {
            core.push_str(&format!("pub fn w{i}() -> i32 {{ hub_x() + hub_y() }}\n"));
        }
        fs::write(dir.path().join("core.rs"), core).unwrap();
        fs::write(
            dir.path().join("lonely.rs"),
            "pub fn obscure_helper() -> i32 { 7 }\n",
        )
        .unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        // Budget = header + the two hub entries exactly, so the hubs fill capacity
        // and no lower-importance node (the helper included) fits without a boost.
        let entry_tokens = |name: &str| {
            let node = g.nodes.iter().find(|n| n.name == name).unwrap();
            crate::obs::count_tokens(&render_entry(&g, node))
        };
        let budget =
            crate::obs::count_tokens(OVERVIEW_HEADER) + entry_tokens("hub_x") + entry_tokens("hub_y");

        let plain = overview(&g, budget, &HashMap::new());
        assert!(
            !plain.contains("`obscure_helper`"),
            "the isolated helper is globally unimportant and must be dropped at the two-hub budget"
        );

        // Mark lonely.rs touched -> overview_seed assigns its node 50.0.
        let touched = vec![dir.path().join("lonely.rs").to_string_lossy().into_owned()];
        let seed = overview_seed(&g, &touched, None);
        assert!(!seed.is_empty(), "touching a real file must seed its node");
        let focused = overview(&g, budget, &seed);
        assert!(
            focused.contains("`obscure_helper`"),
            "marking its file touched must lift the helper into the same budget"
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

    #[test]
    fn score_name_substring_requires_token_boundary() {
        // The measured false-positive: "get" must NOT match mid-token inside a
        // longer identifier, only at a `_`/camelCase boundary.
        let get = tokenize("get");
        assert!(score_name("get_config", &get) > 0, "exact sub-token hits");
        assert!(score_name("getConfig", &get) > 0, "camelCase sub-token hits");
        assert!(score_name("target_getter", &get) > 0, "prefix of a sub-token hits");
        assert_eq!(
            score_name("Widget", &get),
            0,
            "'get' must not match mid-token inside Widget"
        );
        assert_eq!(
            score_name("CONTROL_BUDGET", &get),
            0,
            "'get' must not match inside the BUDGET sub-token"
        );
    }

    #[test]
    fn find_kind_excludes_wrong_kind() {
        // A struct and a function both match "config"; a kind filter must drop the
        // wrong-kind one before it can survive the budget.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("r.rs"),
            "pub struct config_widget;\npub fn config_load() {}\n",
        )
        .unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let funcs = find_kind(&g, "config", 5, Some("function"));
        assert!(funcs.nodes.iter().any(|n| n.name == "config_load"));
        assert!(
            !funcs.nodes.iter().any(|n| n.name == "config_widget"),
            "the struct must be filtered out by the function kind"
        );
        // No filter surfaces both.
        let both = find_kind(&g, "config", 5, None);
        assert!(both.nodes.iter().any(|n| n.name == "config_widget"));
    }

    #[test]
    fn neighbors_dir_splits_callers_and_callees() {
        // a -> b -> c: fan-in of b is {a}, fan-out is {c}.
        let g = rust_graph();
        let b_id = g
            .find_by_name("b", Some("function"))
            .first()
            .unwrap()
            .id
            .clone();
        let callers = neighbors_dir(&g, &b_id, 1, Some("callers"));
        assert!(callers.nodes.iter().any(|n| n.name == "a"), "a calls b");
        assert!(
            !callers.nodes.iter().any(|n| n.name == "c"),
            "c is a callee, not a caller"
        );
        let callees = neighbors_dir(&g, &b_id, 1, Some("callees"));
        assert!(callees.nodes.iter().any(|n| n.name == "c"), "b calls c");
        assert!(
            !callees.nodes.iter().any(|n| n.name == "a"),
            "a is a caller, not a callee"
        );
    }

    #[test]
    fn path_reports_edge_kinds_and_resolves_ambiguity_by_importance() {
        // Two functions named `target`: the a.rs one is called by three fns
        // (structurally important) and reaches `sink`; the b.rs one is isolated.
        // resolve must pick the important one, report 1 other candidate, and the
        // path must carry the `calls` edge kind for each hop.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn target() { sink(); }\n\
             fn sink() {}\n\
             fn u1() { target(); }\n\
             fn u2() { target(); }\n\
             fn u3() { target(); }\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "fn target() {}\n").unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let resp = path(&g, "target", "sink");
        assert!(resp.found, "the important target reaches sink");
        assert_eq!(
            resp.edges.len(),
            resp.path.len().saturating_sub(1),
            "edges align with hops"
        );
        assert!(
            !resp.edges.is_empty() && resp.edges.iter().all(|e| e.kind == "calls"),
            "each hop is a calls edge, kinds surfaced"
        );
        let note = resp
            .resolved
            .iter()
            .find(|r| r.query == "target")
            .expect("ambiguity note for the two `target`s");
        assert_eq!(note.other_candidates, 1, "one other same-name candidate");
        let chosen = g.node(&note.chosen).unwrap();
        assert!(
            chosen.file.ends_with("a.rs"),
            "chose the structurally important target (a.rs), not the isolated b.rs one"
        );
    }

    #[test]
    fn path_unambiguous_omits_resolved_notes() {
        // Unique names -> no ambiguity -> `resolved` stays empty (byte-identical
        // to the pre-change shape after skip_serializing_if).
        let g = rust_graph();
        let resp = path(&g, "a", "c");
        assert!(resp.found);
        assert!(resp.resolved.is_empty(), "unambiguous resolves carry no notes");
    }

    #[test]
    fn query_reports_total_matches_and_resolves_ambiguity_by_importance() {
        // Same fixture shape as `path`'s ambiguity test: two `target`s, one
        // structurally important (three callers), one isolated. `query`
        // (lens_symbol's engine) must surface the same ambiguity note `path`
        // does, and report the pre-limit candidate count only when the `limit`
        // cut actually drops a match.
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("a.rs"),
            "fn target() { sink(); }\n\
             fn sink() {}\n\
             fn u1() { target(); }\n\
             fn u2() { target(); }\n\
             fn u3() { target(); }\n",
        )
        .unwrap();
        fs::write(dir.path().join("b.rs"), "fn target() {}\n").unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let view = query(&g, "target", None, 10, &[]);
        let note = view
            .resolved
            .iter()
            .find(|r| r.query == "target")
            .expect("ambiguity note for the two `target`s");
        assert_eq!(note.other_candidates, 1, "one other same-name candidate");
        let chosen = g.node(&note.chosen).unwrap();
        assert!(chosen.file.ends_with("a.rs"), "chose the important target");
        assert!(
            view.total_matches.is_none(),
            "both matches fit under limit=10, nothing was cut"
        );

        let cut = query(&g, "target", None, 1, &[]);
        assert_eq!(cut.total_matches, Some(2), "limit=1 cut one of the two matches");
    }

    #[test]
    fn find_reports_total_matches_when_limit_cuts_candidates() {
        let dir = tempdir().unwrap();
        let mut src = String::new();
        for i in 0..5 {
            src.push_str(&format!("fn config_load_{i}() {{}}\n"));
        }
        fs::write(dir.path().join("r.rs"), src).unwrap();
        let g = discover(dir.path(), None).unwrap().graph;

        let uncut = find(&g, "config", 10);
        assert!(uncut.total_matches.is_none(), "limit=10 fits all 5 matches");
        let cut = find(&g, "config", 2);
        assert_eq!(cut.total_matches, Some(5), "limit=2 cut 3 of the 5 matches");
    }
}
