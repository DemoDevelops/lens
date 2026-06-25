//! Shared navigation-benchmark logic, `#[path]`-included by `run_navigation.rs`.
//!
//! Measures the graph's per-operation leverage on code-navigation questions — the
//! "fewer tokens + fewer round trips to find the file" claim — WITHOUT a model, so
//! it is fully deterministic. For each question we run a realistic naive path
//! (grep + read the files it would open) and the graph path (one `lens_symbol` /
//! `lens_links` / `lens_path` call), and record both axes:
//!
//!   * **bytes into context** — what the agent has to read either way;
//!   * **round trips** — tool calls. This is the speed metric: each round trip is
//!     a full model-generation + tool-exec cycle (usually the dominant latency), so
//!     collapsing N reads into one graph call is the real "speed" win. Tool CPU
//!     time is microseconds at fixture scale and is deliberately NOT reported (it
//!     would badly understate the per-round-trip model latency it stands in for).
//!
//! Correctness is checked against ground truth authored from the fixture source
//! (not from the graph — that would be circular), so a fast-but-wrong answer fails.
//!
//! Honesty notes baked into the rows: a bare definition lookup is the graph's
//! weakest case — `grep -n` already returns `file:line`, so `lens_symbol` can
//! return *more* bytes (it includes neighbors). The graph wins decisively on
//! callers and reachability, where grep returns ambiguous text matches or cannot
//! answer in one shot at all.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use lens::discovery::graph::Graph;
use lens::discovery::{self, query as gquery};

/// Absolute path to the `benchmarks/` directory, resolved at compile time so the
/// runner works regardless of the current working directory.
pub fn bench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks")
}

/// The fixture repo every question runs against (reused from the accuracy suite;
/// its structure is `main → handle_request → {authenticate, fetch_user →
/// connect_db}`, with crypto (`rotate_keys`, `fingerprint`, `KeyRing`) isolated).
fn fixture_repo() -> PathBuf {
    bench_root().join("accuracy/fixtures/repo")
}

/// One measured navigation question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavRow {
    pub id: String,
    /// "define" | "callers" | "path".
    pub qtype: String,
    pub prompt: String,
    /// Bytes a naive agent loads into context (grep output + files it opens).
    pub naive_bytes: usize,
    /// Bytes the single graph call returns to context.
    pub graph_bytes: usize,
    /// Tool calls the naive path needs.
    pub naive_round_trips: usize,
    /// Tool calls the graph path needs (1).
    pub graph_round_trips: usize,
    /// Did the graph answer match ground truth?
    pub correct: bool,
    /// What the "without graph" path concretely does (the no-strawman note).
    pub baseline: String,
    /// 1-based rank of the answer symbol among same-substring matches a
    /// `lens_symbol` query returns (0 if absent). Records the order importance
    /// ranking imposes, so a ranking change surfaces as a committed-JSON delta.
    pub rank_position: usize,
}

/// The question corpus, authored from the fixture source.
enum Q {
    /// "Which file defines `sym`?" — expects a node `sym` in `file`.
    Define {
        id: &'static str,
        sym: &'static str,
        file: &'static str,
    },
    /// "Which functions call `sym`?" — expects exactly `expect` (sorted).
    Callers {
        id: &'static str,
        sym: &'static str,
        expect: &'static [&'static str],
    },
    /// "Following calls, can `from` reach `to`?" — expects `reachable`.
    Path {
        id: &'static str,
        from: &'static str,
        to: &'static str,
        reachable: bool,
    },
}

fn corpus() -> Vec<Q> {
    vec![
        // define: the graph's weakest case (grep already answers); included for honesty.
        Q::Define {
            id: "nav_define_connect_db",
            sym: "connect_db",
            file: "db.rs",
        },
        Q::Define {
            id: "nav_define_rotate_keys",
            sym: "rotate_keys",
            file: "crypto.rs",
        },
        Q::Define {
            id: "nav_define_authenticate",
            sym: "authenticate",
            file: "auth.rs",
        },
        Q::Define {
            id: "nav_define_connection",
            sym: "Connection",
            file: "db.rs",
        },
        // callers: grep returns ambiguous text matches you must read to disambiguate.
        Q::Callers {
            id: "nav_callers_connect_db",
            sym: "connect_db",
            expect: &["fetch_user"],
        },
        Q::Callers {
            id: "nav_callers_fetch_user",
            sym: "fetch_user",
            expect: &["handle_request"],
        },
        Q::Callers {
            id: "nav_callers_authenticate",
            sym: "authenticate",
            expect: &["handle_request"],
        },
        // path: grep cannot answer multi-hop reachability in one shot.
        Q::Path {
            id: "nav_path_handle_to_connect",
            from: "handle_request",
            to: "connect_db",
            reachable: true,
        },
        Q::Path {
            id: "nav_path_main_to_connect",
            from: "main",
            to: "connect_db",
            reachable: true,
        },
        Q::Path {
            id: "nav_path_handle_to_rotate",
            from: "handle_request",
            to: "rotate_keys",
            reachable: false,
        },
        Q::Path {
            id: "nav_path_connect_to_rotate",
            from: "connect_db",
            to: "rotate_keys",
            reachable: false,
        },
    ]
}

/// Build the fixture graph once, then evaluate every question against it.
/// Deterministic: same committed fixture → same numbers.
pub fn compute_navigation() -> anyhow::Result<Vec<NavRow>> {
    let repo = fixture_repo();
    let outcome = discovery::discover(&repo, None)?;
    let g = &outcome.graph;
    corpus().into_iter().map(|q| eval(&repo, g, q)).collect()
}

fn eval(repo: &Path, g: &Graph, q: Q) -> anyhow::Result<NavRow> {
    match q {
        Q::Define { id, sym, file } => {
            // Naive: grep the symbol, then open the defining file to read it.
            let (grep_out, _) = grep(repo, sym);
            let def_bytes = std::fs::read_to_string(repo.join(file))?.len();
            let view = gquery::query(g, sym, None, 20, &[]);
            let graph_bytes = serde_json::to_string(&view)?.len();
            let correct = view
                .nodes
                .iter()
                .any(|n| n.name == sym && n.file.ends_with(file));
            Ok(NavRow {
                id: id.into(),
                qtype: "define".into(),
                prompt: format!("Which file defines `{sym}`?"),
                naive_bytes: grep_out.len() + def_bytes,
                graph_bytes,
                naive_round_trips: 2, // grep + open the defining file
                graph_round_trips: 1,
                correct,
                baseline: "grep the symbol, then open the defining file to read it".into(),
                rank_position: rank_of(g, sym),
            })
        }
        Q::Callers { id, sym, expect } => {
            // Naive: grep the symbol, then read every matched file to tell real
            // calls from definitions/mentions/imports.
            let (grep_out, files) = grep(repo, sym);
            let read: usize = files
                .iter()
                .filter_map(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.len())
                .sum();
            let (graph_bytes, mut callers) = match node_id(g, sym) {
                Some(nid) => {
                    let view = gquery::neighbors(g, &nid, 1);
                    let bytes = serde_json::to_string(&view)?.len();
                    let names: Vec<String> = view
                        .edges
                        .iter()
                        .filter(|e| e.to == nid && e.kind == "calls")
                        .filter_map(|e| view.nodes.iter().find(|n| n.id == e.from))
                        .map(|n| n.name.clone())
                        .collect();
                    (bytes, names)
                }
                None => (0, vec![]),
            };
            callers.sort();
            callers.dedup();
            let mut want: Vec<String> = expect.iter().map(|s| s.to_string()).collect();
            want.sort();
            Ok(NavRow {
                id: id.into(),
                qtype: "callers".into(),
                prompt: format!("Which functions call `{sym}`?"),
                naive_bytes: grep_out.len() + read,
                graph_bytes,
                naive_round_trips: 1 + files.len(),
                graph_round_trips: 1,
                correct: callers == want,
                baseline:
                    "grep the symbol, then read each matched file to tell real calls from mentions"
                        .into(),
                rank_position: rank_of(g, sym),
            })
        }
        Q::Path {
            id,
            from,
            to,
            reachable,
        } => {
            // Naive: read the source subtree to trace call edges by hand — no single
            // grep gives multi-hop reachability. Exact for non-reachable (you must
            // exhaust the component); an upper bound for lucky positives.
            let files = source_files(repo);
            let naive_bytes: usize = files
                .iter()
                .filter_map(|p| std::fs::read_to_string(p).ok())
                .map(|s| s.len())
                .sum();
            let resp = gquery::path(g, from, to);
            let graph_bytes = serde_json::to_string(&resp)?.len();
            let correct = if reachable {
                resp.found
                    && resp.path.first().map(|n| n.name == from).unwrap_or(false)
                    && resp.path.last().map(|n| n.name == to).unwrap_or(false)
            } else {
                !resp.found
            };
            Ok(NavRow {
                id: id.into(),
                qtype: "path".into(),
                prompt: format!("Following calls, can `{from}` reach `{to}`?"),
                naive_bytes,
                graph_bytes,
                naive_round_trips: files.len(),
                graph_round_trips: 1,
                correct,
                baseline: "read the source subtree to trace call edges by hand".into(),
                rank_position: rank_of(g, from),
            })
        }
    }
}

/// First node whose name is exactly `sym` (find_by_name is a substring match).
fn node_id(g: &Graph, sym: &str) -> Option<String> {
    g.find_by_name(sym, None)
        .into_iter()
        .find(|n| n.name == sym)
        .map(|n| n.id.clone())
}

/// 1-based rank of the symbol named `name` among the name-matching nodes a
/// `lens_symbol` query returns (0 if absent). With unique fixture names this is
/// 1, but the field lets an importance-ranking change show up as a JSON delta.
fn rank_of(g: &Graph, name: &str) -> usize {
    let lname = name.to_ascii_lowercase();
    gquery::query(g, name, None, 20, &[])
        .nodes
        .iter()
        .filter(|n| n.name.to_ascii_lowercase().contains(&lname))
        .position(|n| n.name == name)
        .map(|p| p + 1)
        .unwrap_or(0)
}

/// Files under `repo`, sorted for determinism.
fn source_files(repo: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = walkdir::WalkDir::new(repo)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    v.sort();
    v
}

/// Deterministic grep: every line containing `needle`, emitted `relpath:line:text`,
/// plus the set of files with at least one match. Case-sensitive substring, files
/// in sorted order, so output bytes are reproducible run-to-run.
fn grep(repo: &Path, needle: &str) -> (String, Vec<PathBuf>) {
    let mut out = String::new();
    let mut files = Vec::new();
    for path in source_files(repo) {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let rel = path.strip_prefix(repo).unwrap_or(&path).to_string_lossy();
        let mut hit = false;
        for (i, line) in content.lines().enumerate() {
            if line.contains(needle) {
                out.push_str(&format!("{}:{}:{}\n", rel, i + 1, line));
                hit = true;
            }
        }
        if hit {
            files.push(path);
        }
    }
    (out, files)
}

fn pct(naive: usize, graph: usize) -> i64 {
    if naive == 0 {
        return 0;
    }
    (((naive as f64 - graph as f64) / naive as f64) * 100.0).round() as i64
}

/// Render the per-type aggregate, the per-question detail, and the methodology.
pub fn render_navigation_markdown(rows: &[NavRow]) -> String {
    let mut s = String::new();

    s.push_str("### Graph leverage by question type (deterministic, no model)\n\n");
    s.push_str("| Question type | N | Naive bytes | Graph bytes | Bytes saved | Naive round-trips | Graph round-trips | Correct |\n");
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    for (qtype, label) in [
        ("define", "Definition lookup"),
        ("callers", "Who-calls"),
        ("path", "Reachability"),
    ] {
        let group: Vec<&NavRow> = rows.iter().filter(|r| r.qtype == qtype).collect();
        if group.is_empty() {
            continue;
        }
        let n = group.len();
        let nb: usize = group.iter().map(|r| r.naive_bytes).sum();
        let gb: usize = group.iter().map(|r| r.graph_bytes).sum();
        let nrt: usize = group.iter().map(|r| r.naive_round_trips).sum();
        let grt: usize = group.iter().map(|r| r.graph_round_trips).sum();
        let ok = group.iter().filter(|r| r.correct).count();
        s.push_str(&format!(
            "| {} | {} | {} | {} | {}% | {} | {} | {}/{} |\n",
            label,
            n,
            nb,
            gb,
            pct(nb, gb),
            nrt,
            grt,
            ok,
            n,
        ));
    }

    s.push_str("\n### Per-question detail\n\n");
    s.push_str("| ID | Type | Naive bytes | Graph bytes | Naive RT | Graph RT | Correct | Without the graph, the agent… |\n");
    s.push_str("| --- | --- | ---: | ---: | ---: | ---: | :---: | --- |\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
            r.id,
            r.qtype,
            r.naive_bytes,
            r.graph_bytes,
            r.naive_round_trips,
            r.graph_round_trips,
            if r.correct { "✓" } else { "✗" },
            r.baseline,
        ));
    }

    s.push_str(concat!(
        "\n**Reading these numbers.**\n",
        "- **Round-trips is the speed metric.** Each tool call is a full model-generation + tool-exec cycle, usually the dominant latency. A graph call answers in 1 round trip what the naive path needs several for. Tool CPU time (microseconds at this fixture size) is not reported — it would understate the per-round-trip model latency it stands in for.\n",
        "- **Definition lookup is the graph's weakest case, shown honestly.** `grep -n` already returns `file:line`, so `lens_symbol` can return *more* bytes (it bundles the symbol's neighbors). The graph's win is on who-calls and reachability, not bare lookup.\n",
        "- **Who-calls:** grep returns every textual match (definition, imports, comments, real calls); the naive path reads each matched file to disambiguate, while `lens_links` returns the exact call edges in one call.\n",
        "- **Reachability:** grep cannot answer a multi-hop \"does A reach B\" in one shot; the naive baseline reads the source subtree to trace edges by hand. `lens_path` returns the path (or proves none) in one call.\n",
        "- **Scale caveat:** the fixture is 5 files, so absolute bytes are small; the round-trip and disambiguation-read *ratios* are the signal, and they grow with repo size.\n",
    ));
    s
}
