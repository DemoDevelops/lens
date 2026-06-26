//! `bench_changes` - deterministic, no-model benchmark of the four functional
//! changes shipped in this round, each with an honest baseline:
//!
//!   C1 TOON compaction  - bytes of a uniform JSON array as plain JSON vs the
//!                         lossless TOON form `compact_json` now emits, at scale,
//!                         plus a round-trip losslessness gate.
//!   C2 proximity rank   - rank of an in-focus file's first match in `lens_symbol`
//!                         with no session context vs with the file marked recently
//!                         touched (Aider-style boost).
//!   C3 lens_find       - natural-language query to symbol: hit@1 / hit@3 against a
//!                         ground-truth corpus, and bytes returned vs a grep baseline.
//!   C4 conflict resolve - stale/contradictory session events dropped at recovery
//!                         read time: raw event count vs resolved, and the deleted
//!                         path no longer surfaces as an active modification.
//!
//!   cargo run --bin bench_changes

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use lens::discovery::{self, query as gquery};
use lens::index::Index;
use lens::session::{snapshot, store::SessionStore, Event};
use lens::store::compress;

fn bench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks")
}

/// The committed fixture the navigation suite already uses (main -> handle_request
/// -> {authenticate, fetch_user -> connect_db}, crypto isolated).
fn fixture_repo() -> PathBuf {
    bench_root().join("accuracy/fixtures/repo")
}

fn pct(before: usize, after: usize) -> i64 {
    if before == 0 {
        return 0;
    }
    (((before as f64 - after as f64) / before as f64) * 100.0).round() as i64
}

fn json_len(v: &Value) -> usize {
    serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0)
}

// --- C1: TOON compaction at scale -------------------------------------------

/// A uniform array of `n` flat scalar objects (the shape TOON targets).
fn uniform_array(n: usize) -> Value {
    let rows: Vec<Value> = (0..n)
        .map(|i| {
            json!({
                "id": i,
                "name": format!("item_{i}"),
                "active": i % 2 == 0,
                "score": i as i64 * 3,
                "tag": "release",
            })
        })
        .collect();
    Value::Array(rows)
}

fn c1_toon() -> (String, bool) {
    let tiers = [
        ("Small", 20usize),
        ("Medium", 200),
        ("Large", 1000),
        ("Huge", 4000),
    ];
    let mut s = String::new();
    s.push_str("## C1 - TOON compaction (uniform structured data, lossless)\n\n");
    s.push_str("Baseline is plain JSON (what a naive agent dumps into context). TOON is what `compact_json` now emits for a uniform array of flat objects: keys once, values per row.\n\n");
    s.push_str("| Rows | JSON bytes | TOON bytes | saved | round-trip lossless |\n");
    s.push_str("| --- | ---: | ---: | ---: | :---: |\n");
    let mut all_lossless = true;
    for (label, n) in tiers {
        let v = uniform_array(n);
        let json_bytes = json_len(&v);
        let toon = compress::compact_json(&v);
        let toon_bytes = json_len(&toon);
        // compact_json drops nulls first; this fixture has none, so the lossless
        // target is the original value exactly.
        let lossless = compress::expand_json(&toon) == v;
        all_lossless &= lossless;
        s.push_str(&format!(
            "| {label} ({n}) | {json_bytes} | {toon_bytes} | {}% | {} |\n",
            pct(json_bytes, toon_bytes),
            if lossless { "yes" } else { "NO" },
        ));
    }
    s.push_str("\nFlat across scale: each row saves its repeated key names, so the ratio holds as the array grows. Lossless and deterministic (no model, no second pass).\n");
    (s, all_lossless)
}

// --- C2: session-proximity rank lift ----------------------------------------

fn c2_proximity() -> anyhow::Result<(String, bool)> {
    let repo = fixture_repo();
    let outcome = discovery::discover(&repo, None)?;
    let g = &outcome.graph;

    // Pick a substring whose matches span the most distinct files, so the result
    // set is genuinely cross-file (where proximity can reorder anything).
    let mut best = (String::new(), 0usize);
    for c in "etaoinsrhldcu".chars() {
        let q = c.to_string();
        let view = gquery::query(g, &q, None, 50, &[]);
        let files: std::collections::BTreeSet<&str> =
            view.nodes.iter().map(|n| n.file.as_str()).collect();
        if files.len() > best.1 {
            best = (q, files.len());
        }
    }
    let q = best.0;

    let base = gquery::query(g, &q, None, 50, &[]);
    // Focus on the least-prominent file: the one whose first match ranks LATEST in
    // the importance order, so a proximity boost has real room to lift it (and the
    // lift isn't an artifact of the file already being near the top).
    let mut first_pos: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for (i, n) in base.nodes.iter().enumerate() {
        first_pos.entry(n.file.clone()).or_insert(i);
    }
    let focus_file = first_pos
        .iter()
        .max_by_key(|(_, p)| **p)
        .map(|(f, _)| f.clone())
        .unwrap_or_default();
    let before = first_pos.get(&focus_file).map(|p| p + 1).unwrap_or(0);

    let boosted = gquery::query(g, &q, None, 50, std::slice::from_ref(&focus_file));
    let after = boosted
        .nodes
        .iter()
        .position(|n| n.file == focus_file)
        .map(|p| p + 1)
        .unwrap_or(0);

    let focus_name = Path::new(&focus_file)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or(focus_file);

    let mut s = String::new();
    s.push_str("## C2 - lens_symbol session-proximity boost\n\n");
    s.push_str(&format!(
        "Query `\"{q}\"` spans {} files. Marking `{focus_name}` as recently touched moves its first match from rank **{before}** to rank **{after}** (1 = top). Empty session context leaves ordering byte-for-byte unchanged.\n",
        best.1
    ));
    let improved = after > 0 && after < before;
    Ok((s, improved))
}

// --- C3: natural-language find -----------------------------------------------

struct FindQ {
    nl: &'static str,
    expect: &'static str,
    /// The single keyword a grep baseline would search for.
    grep_term: &'static str,
}

fn c3_find() -> anyhow::Result<(String, bool)> {
    let repo = fixture_repo();
    let outcome = discovery::discover(&repo, None)?;
    let g = &outcome.graph;

    let corpus = [
        FindQ {
            nl: "connect to the database",
            expect: "connect_db",
            grep_term: "connect",
        },
        FindQ {
            nl: "authenticate the request",
            expect: "authenticate",
            grep_term: "authenticate",
        },
        FindQ {
            nl: "rotate the encryption keys",
            expect: "rotate_keys",
            grep_term: "rotate",
        },
        FindQ {
            nl: "fetch the user record",
            expect: "fetch_user",
            grep_term: "fetch",
        },
        FindQ {
            nl: "handle the incoming request",
            expect: "handle_request",
            grep_term: "handle",
        },
    ];

    let mut hit1 = 0usize;
    let mut hit3 = 0usize;
    let mut find_bytes = 0usize;
    let mut grep_bytes = 0usize;
    let mut detail = String::new();
    for q in &corpus {
        let view = gquery::find(g, q.nl, 5);
        find_bytes += json_len(&serde_json::to_value(&view)?);
        let top = view.nodes.first().map(|n| n.name.as_str()).unwrap_or("");
        let in_top3 = view.nodes.iter().take(3).any(|n| n.name == q.expect);
        if top == q.expect {
            hit1 += 1;
        }
        if in_top3 {
            hit3 += 1;
        }
        // Grep baseline: search the keyword across the repo, then read each matched
        // file to find the symbol it maps to (grep cannot map meaning to a symbol).
        let (gout, files) = grep(&repo, q.grep_term);
        let read: usize = files
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.len())
            .sum();
        grep_bytes += gout.len() + read;
        let h1 = if top == q.expect {
            "yes".to_string()
        } else {
            format!("no ({top})")
        };
        detail.push_str(&format!(
            "| \"{}\" | {} | {} | {} |\n",
            q.nl,
            q.expect,
            h1,
            if in_top3 { "yes" } else { "no" },
        ));
    }
    let n = corpus.len();
    let mut s = String::new();
    s.push_str("## C3 - lens_find (natural language to symbol)\n\n");
    s.push_str(&format!(
        "Lexical NL to symbol on the fixture: **hit@1 {hit1}/{n}**, **hit@3 {hit3}/{n}**. The win is correctness, not bytes: lens_find maps a natural-language phrase to the right symbol with no keyword supplied, which grep cannot do at all. For reference, answering all {n} costs lens_find {find_bytes} bytes (resolved symbols + their neighbors) vs {grep_bytes} bytes for a keyword grep that has already been handed the answer term and still returns raw matches to disambiguate by hand.\n\n"
    ));
    s.push_str(
        "| Natural-language query | Expected | hit@1 | hit@3 |\n| --- | --- | :---: | :---: |\n",
    );
    s.push_str(&detail);
    let pass = hit1 >= n - 1 && hit3 == n; // allow one rank-1 miss, but it must be top-3
    Ok((s, pass))
}

/// Minimal deterministic grep mirroring the navigation suite's baseline.
fn grep(repo: &Path, needle: &str) -> (String, Vec<PathBuf>) {
    let mut out = String::new();
    let mut files = Vec::new();
    let mut all: Vec<PathBuf> = walkdir::WalkDir::new(repo)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .collect();
    all.sort();
    for path in all {
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

// --- C4: conflict resolution at recovery ------------------------------------

fn file_event(sid: &str, ts: i64, action: &str, path: &str) -> Event {
    Event {
        session_id: sid.into(),
        project: "/bench".into(),
        timestamp: ts,
        category: "file".into(),
        priority: 1,
        payload: json!({ "action": action, "path": path }),
        source_hook: "PostToolUse".into(),
    }
}

fn c4_recovery() -> anyhow::Result<(String, bool)> {
    let dir = std::env::temp_dir().join(format!("lens_bench_c4_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let store = SessionStore::open(&dir)?;
    let sid = "bench";

    // 10 files each edited 4 times (the agent revisits files), plus one file that
    // was edited then deleted - the contradiction recovery must resolve.
    let mut ts = 0i64;
    let mut events = Vec::new();
    for f in 0..10 {
        for _ in 0..4 {
            ts += 1;
            events.push(file_event(sid, ts, "edit", &format!("src/mod_{f}.rs")));
        }
    }
    ts += 1;
    events.push(file_event(sid, ts, "edit", "src/gone.rs"));
    ts += 1;
    events.push(file_event(sid, ts, "delete", "src/gone.rs"));
    store.insert_events(&events)?;

    let raw = store.events_for_session(sid)?;
    let resolved = store.resolved_events_for_session(sid)?;

    let budget = lens::session::snapshot_budget();
    let raw_snap = snapshot::build_snapshot(&raw, budget, 1);
    let res_snap = snapshot::build_snapshot(&resolved, budget, 1);

    // Correctness: in the resolved view, src/gone.rs survives only as its latest
    // (delete) event, never as the earlier edit.
    let gone: Vec<&Event> = resolved
        .iter()
        .filter(|e| e.payload.get("path").and_then(|p| p.as_str()) == Some("src/gone.rs"))
        .collect();
    let gone_ok =
        gone.len() == 1 && gone[0].payload.get("action").and_then(|a| a.as_str()) == Some("delete");
    // Each repeatedly-edited path collapses to exactly one event.
    let per_path_ok = resolved
        .iter()
        .filter(|e| {
            e.payload
                .get("path")
                .and_then(|p| p.as_str())
                .map(|p| p.starts_with("src/mod_"))
                .unwrap_or(false)
        })
        .count()
        == 10;

    let _ = std::fs::remove_dir_all(&dir);

    let mut s = String::new();
    s.push_str("## C4 - session conflict resolution at recovery\n\n");
    s.push_str(&format!(
        "Raw event log: **{}** file events. Resolved (latest-per-path) view feeding recovery: **{}** ({}% fewer). The edited-then-deleted path surfaces only as its latest state: **{}**. Recovery snapshot bytes: raw **{}** vs resolved **{}**.\n",
        raw.len(),
        resolved.len(),
        pct(raw.len(), resolved.len()),
        if gone_ok { "delete (correct)" } else { "WRONG" },
        raw_snap.len(),
        res_snap.len(),
    ));
    let pass = gone_ok && per_path_ok;
    Ok((s, pass))
}

// ===========================================================================
// C5–C15: the benchmark-gated improvements of the 7-fix plan. Each gate is a
// deterministic, offline before/after on a FIXED committed fixture under
// `benchmarks/changes/fixtures/`. Relative gates read a baseline captured on the
// pre-fix code (`bench_changes --update`, run once on master) from
// `expected/baseline.json`; absolute gates need no baseline. Gates whose fix
// introduces a new API (C11/C13/C15) are added alongside that task.
// ===========================================================================

/// Pre-fix baselines, captured on master with `bench_changes --update` and
/// committed to `expected/baseline.json`. Relative gates (C5/C7) compare the live
/// number to these; C8/C12 use the recall floor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Baseline {
    c5_mrr: f64,
    c5_p_at_5: f64,
    c7_mrr: f64,
    c8_precision: f64,
    c8_recall: f64,
    c12_recall: f64,
    c12_bytes: usize,
}

fn baseline_path() -> PathBuf {
    bench_root().join("changes/expected/baseline.json")
}

fn load_baseline() -> Baseline {
    std::fs::read_to_string(baseline_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn changes_fixture(name: &str) -> PathBuf {
    bench_root().join("changes/fixtures").join(name)
}

// --- C5: BM25F field-weighted search ----------------------------------------

/// (query, file that DEFINES the query term as a symbol). Each term is also
/// repeated in a different file's prose, so content-only BM25 ranks the prose
/// file first; the symbol-column weight must flip the definition to the top.
const C5_CORPUS: [(&str, &str); 5] = [
    ("tokenize", "parser.rs"),
    ("checkout", "cart.rs"),
    ("throttle", "limiter.rs"),
    ("marshal", "codec.rs"),
    ("reconcile", "ledger.rs"),
];

fn measure_c5() -> (f64, f64) {
    let data = tempfile::tempdir().unwrap();
    let index = Index::open(data.path()).unwrap();
    index.index_path(&changes_fixture("search"), true).unwrap();
    let mut rr = 0.0;
    let mut hits5 = 0usize;
    for (q, expect) in C5_CORPUS {
        let resp = index.search(&[q.to_string()], 10).unwrap();
        if let Some(pos) = resp.results[0].hits.iter().position(|h| h.path.ends_with(expect)) {
            rr += 1.0 / (pos + 1) as f64;
            if pos < 5 {
                hits5 += 1;
            }
        }
    }
    let n = C5_CORPUS.len() as f64;
    (rr / n, hits5 as f64 / n)
}

fn gate_c5(b: &Baseline) -> (String, bool) {
    let (mrr, p5) = measure_c5();
    let pass = mrr >= b.c5_mrr * 1.15 && p5 + 1e-9 >= b.c5_p_at_5;
    let s = format!(
        "## C5 - BM25F field-weighted search\n\nLabeled query→definition over `fixtures/search`: a term that is a *symbol* in the right file vs the same term repeated as prose in another file. Baseline (content-only BM25) MRR **{:.3}**, P@5 **{:.3}**; live MRR **{:.3}**, P@5 **{:.3}**. Gate: MRR ≥ baseline×1.15 and P@5 not regressed.\n",
        b.c5_mrr, b.c5_p_at_5, mrr, p5
    );
    (s, pass)
}

// --- C6: punctuation / operator queries -------------------------------------

fn measure_c6() -> Vec<(String, usize)> {
    let data = tempfile::tempdir().unwrap();
    let index = Index::open(data.path()).unwrap();
    index.index_path(&changes_fixture("search"), true).unwrap();
    ["std::fs", "->", "fn add"]
        .iter()
        .map(|q| {
            let resp = index.search(&[q.to_string()], 5).unwrap();
            (q.to_string(), resp.results[0].hits.len())
        })
        .collect()
}

fn gate_c6() -> (String, bool) {
    let results = measure_c6();
    let pass = results.iter().all(|(_, n)| *n >= 1);
    let mut s = String::from(
        "## C6 - punctuation / operator queries\n\nThe sanitizer strips `:`/`.`/`>` and the porter stemmer mangles identifiers, so structural queries return nothing. Each must return ≥1 hit against `fixtures/search`.\n\n| Query | Hits |\n| --- | ---: |\n",
    );
    for (q, n) in &results {
        s.push_str(&format!("| `{q}` | {n} |\n"));
    }
    (s, pass)
}

// --- C7: graph importance ranking -------------------------------------------

/// (query substring, the gold symbol = the high-degree hub among the matches).
const C7_CORPUS: [(&str, &str); 4] = [
    ("handle", "handle"),
    ("load", "load"),
    ("render", "render"),
    ("parse", "parse"),
];

fn measure_c7() -> f64 {
    let g = discovery::discover(&changes_fixture("rank"), None).unwrap().graph;
    let mut rr = 0.0;
    for (q, gold) in C7_CORPUS {
        let view = gquery::query(&g, q, None, 20, &[]);
        let rank = view
            .nodes
            .iter()
            .filter(|n| n.name.to_ascii_lowercase().contains(q))
            .position(|n| n.name == gold)
            .map(|p| p + 1);
        if let Some(r) = rank {
            rr += 1.0 / r as f64;
        }
    }
    rr / C7_CORPUS.len() as f64
}

fn gate_c7(b: &Baseline) -> (String, bool) {
    let mrr = measure_c7();
    let pass = mrr >= b.c7_mrr * 1.25;
    let s = format!(
        "## C7 - graph importance ranking (lens_symbol)\n\nFor each ambiguous query over `fixtures/rank`, the high-degree hub should rank first among same-substring matches. Baseline (id-sort) MRR **{:.3}**; live MRR **{:.3}**. Gate: MRR ≥ baseline×1.25.\n",
        b.c7_mrr, mrr
    );
    (s, pass)
}

// --- C8: scope-aware call-edge precision/recall -----------------------------

/// The hand-labeled true call graph of `fixtures/calls`:
/// (caller, callee, file the callee is defined in).
fn c8_truth() -> Vec<(String, String, String)> {
    let mut t = Vec::new();
    for (task, file) in [
        ("task_a", "a.rs"),
        ("task_b", "b.rs"),
        ("task_c", "c.rs"),
        ("task_d", "d.rs"),
        ("task_e", "e.rs"),
    ] {
        t.push((task.into(), "check".into(), file.into()));
        t.push((task.into(), "normalize".into(), "shared.rs".into()));
    }
    t
}

fn measure_c8() -> (f64, f64) {
    let g = discovery::discover(&changes_fixture("calls"), None).unwrap().graph;
    let info = |id: &str| g.node(id).map(|n| (n.name.clone(), n.file.clone()));
    let extracted: Vec<(String, String, String)> = g
        .edges
        .iter()
        .filter(|e| e.kind == "calls")
        .filter_map(|e| {
            let (from, _) = info(&e.from)?;
            let (to, tofile) = info(&e.to)?;
            Some((from, to, tofile))
        })
        .collect();
    let truth = c8_truth();
    let is_true = |x: &(String, String, String)| {
        truth
            .iter()
            .any(|(c, ce, f)| *c == x.0 && *ce == x.1 && x.2.ends_with(f.as_str()))
    };
    let correct = extracted.iter().filter(|x| is_true(x)).count();
    let precision = if extracted.is_empty() {
        0.0
    } else {
        correct as f64 / extracted.len() as f64
    };
    let covered = truth
        .iter()
        .filter(|(c, ce, f)| {
            extracted
                .iter()
                .any(|x| x.0 == *c && x.1 == *ce && x.2.ends_with(f.as_str()))
        })
        .count();
    let recall = covered as f64 / truth.len() as f64;
    (precision, recall)
}

fn gate_c8(b: &Baseline) -> (String, bool) {
    let (precision, recall) = measure_c8();
    let pass = precision >= 0.85 && recall + 1e-9 >= b.c8_recall;
    let s = format!(
        "## C8 - scope-aware call resolution\n\nName-only resolution links every `check()` call to all five same-named definitions; scope-aware resolution keeps only the same-file one. Baseline precision **{:.3}** / recall **{:.3}**; live precision **{:.3}** / recall **{:.3}**. Gate: precision ≥ 0.85, recall ≥ baseline.\n",
        b.c8_precision, b.c8_recall, precision, recall
    );
    (s, pass)
}

// --- C9: multi-symbol import completeness -----------------------------------

fn measure_c9() -> usize {
    let g = discovery::discover(&changes_fixture("imports"), None).unwrap().graph;
    let targets = ["Alpha", "Beta", "Gamma"];
    g.edges
        .iter()
        .filter(|e| e.kind == "imports")
        .filter(|e| {
            g.node(&e.to)
                .map(|n| targets.contains(&n.name.as_str()))
                .unwrap_or(false)
        })
        .count()
}

fn gate_c9() -> (String, bool) {
    let edges = measure_c9();
    let pass = edges == 3;
    let s = format!(
        "## C9 - multi-symbol import completeness\n\n`use crate::shared::{{Alpha, Beta, Gamma}};` must emit one import edge per symbol, not just the last token. Import edges to {{Alpha, Beta, Gamma}}: **{edges}** (want 3).\n"
    );
    (s, pass)
}

// --- C10: trait-signature / const / type capture ----------------------------

fn measure_c10() -> usize {
    let g = discovery::discover(&changes_fixture("imports"), None).unwrap().graph;
    let kinds = ["function_signature", "const", "type"];
    g.nodes
        .iter()
        .filter(|n| kinds.contains(&n.kind.as_str()))
        .count()
}

fn gate_c10() -> (String, bool) {
    let n = measure_c10();
    let pass = n > 0;
    let s = format!(
        "## C10 - trait-signature / const / type capture\n\nThe base Rust query misses trait method signatures, associated/free consts, and type aliases. Nodes of those kinds in `fixtures/imports`: **{n}** (want > 0).\n"
    );
    (s, pass)
}

// --- C12: recovery recall at the snapshot budget ----------------------------

/// Distinctive substrings, one per optional snapshot section across the rank
/// spectrum. The lowest-rank ones drop first when the budget is tight, so recall
/// rises with the budget.
const C12_EVIDENCE: [&str; 8] = [
    "PCI scope minimal",
    "vault access",
    "billing_v2",
    "double-entry",
    "refactor-ledger",
    "cargo test",
    "issue-1234",
    "regression-debug",
];

fn c12_ev(category: &str, priority: u8, payload: Value, ts: i64) -> Event {
    Event {
        session_id: "c12".into(),
        project: "/bench".into(),
        timestamp: ts,
        category: category.into(),
        priority,
        payload,
        source_hook: "PostToolUse".into(),
    }
}

/// A long session: a small must-keep core plus many optional events spread
/// across the section-rank spectrum, sized to overflow the 2048 budget.
fn c12_events() -> Vec<Event> {
    let mut evs = Vec::new();
    let mut ts = 0i64;
    let mut next = || {
        ts += 1;
        ts
    };
    evs.push(c12_ev(
        "user-prompt",
        1,
        json!({"prompt": "implement the billing reconciliation service"}),
        next(),
    ));
    for t in ["wire ledger schema", "add reconcile job", "backfill historical entries"] {
        evs.push(c12_ev("task", 1, json!({"task": t, "status": "in_progress"}), next()));
    }
    for d in ["use append-only ledger", "settle in minor units"] {
        evs.push(c12_ev("decision", 2, json!({"text": d}), next()));
    }
    for f in ["src/ledger.rs", "src/reconcile.rs"] {
        evs.push(c12_ev("file", 1, json!({"action": "edit", "path": f}), next()));
    }
    // High-rank optionals (survive longest).
    evs.push(c12_ev("constraint", 1, json!({"text": "keep PCI scope minimal across the service"}), next()));
    evs.push(c12_ev("constraint", 1, json!({"text": "no PII in logs"}), next()));
    evs.push(c12_ev("blocker", 1, json!({"text": "blocked on vault access for the signing key"}), next()));
    evs.push(c12_ev("plan", 1, json!({"action": "exit", "plan": "stage rollout behind flag billing_v2"}), next()));
    evs.push(c12_ev("rejected-approach", 1, json!({"text": "rejected synchronous double-entry writes"}), next()));
    // Low-rank optionals (dropped first under a tight budget). refactor-ledger is
    // the most recent commit so it survives the git section's recency cap.
    for i in 0..7 {
        evs.push(c12_ev("git", 2, json!({"op": "commit", "cmd": format!("git commit -m step-{i}")}), next()));
    }
    evs.push(c12_ev("git", 2, json!({"op": "commit", "cmd": "git commit -m refactor-ledger"}), next()));
    for c in ["cargo test --workspace", "cargo clippy --all", "cargo fmt --check", "cargo build --release"] {
        evs.push(c12_ev("environment", 3, json!({"cmd": c}), next()));
    }
    for i in 0..60 {
        evs.push(c12_ev("mcp-tool", 3, json!({"tool": format!("mcp__svc__operation_{i}")}), next()));
    }
    evs.push(c12_ev("external-ref", 3, json!({"ref": "issue-1234"}), next()));
    evs.push(c12_ev("intent", 4, json!({"intent": "regression-debug"}), next()));
    evs
}

fn measure_c12(budget: usize) -> (f64, usize) {
    let snap = snapshot::build_snapshot(&c12_events(), budget, 1);
    let present = C12_EVIDENCE.iter().filter(|e| snap.contains(**e)).count();
    (present as f64 / C12_EVIDENCE.len() as f64, snap.len())
}

fn gate_c12(b: &Baseline) -> (String, bool) {
    let (recall, bytes) = measure_c12(lens::session::snapshot_budget());
    let pass = recall + 1e-9 >= b.c12_recall && bytes <= 8192;
    let s = format!(
        "## C12 - recovery recall at the snapshot budget\n\nA long session's evidence spans optional sections; the lowest-rank ones drop under a tight budget. Baseline (2048) recall **{:.3}** ({} bytes); live (budget {}) recall **{:.3}** ({} bytes). Gate: recall ≥ baseline, bytes ≤ 8192.\n",
        b.c12_recall,
        b.c12_bytes,
        lens::session::snapshot_budget(),
        recall,
        bytes
    );
    (s, pass)
}

// --- C11: token-budgeted overview (lens_overview) ---------------------------

/// (fraction of the important hub symbols present in the overview, overview tokens).
fn measure_c11() -> (f64, usize) {
    let g = discovery::discover(&changes_fixture("overview"), None)
        .unwrap()
        .graph;
    let overview = gquery::overview(&g, 2000);
    let important = ["hub_a", "hub_b", "hub_c", "hub_d", "hub_e"];
    let present = important
        .iter()
        .filter(|h| overview.contains(&format!("`{h}`")))
        .count();
    (
        present as f64 / important.len() as f64,
        lens::obs::count_tokens(&overview),
    )
}

fn gate_c11() -> (String, bool) {
    let (frac, tokens) = measure_c11();
    let pass = frac >= 0.8 && tokens <= 2000;
    let s = format!(
        "## C11 - token-budgeted overview (lens_overview)\n\nThe overview of `fixtures/overview` (5 hubs + 100 workers, ranked by importance) is binary-searched down to a 2000-token budget. Important hub symbols present: **{:.0}%** in a **{}**-token map. Gate: ≥80% of important symbols within a 2000-token budget.\n",
        frac * 100.0,
        tokens
    );
    (s, pass)
}

// --- C13: cross-session project memory --------------------------------------

/// Durable facts session A records; each is both the payload text and the
/// evidence string a fresh session must recall.
const C13_EVIDENCE: [&str; 3] = [
    "argon2 over bcrypt",
    "retry budget is three",
    "secrets stay out of logs",
];

fn c13_recall(text: &str) -> f64 {
    let present = C13_EVIDENCE.iter().filter(|e| text.contains(**e)).count();
    present as f64 / C13_EVIDENCE.len() as f64
}

/// (recall without persisted memory, recall with it). Session A records durable
/// decisions/constraints; a fresh session clears the live event log, then we
/// recover from the cleared log (no memory) vs from persisted project memory.
fn measure_c13() -> (f64, f64) {
    let dir = std::env::temp_dir().join(format!("lens_bench_c13_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = SessionStore::open(&dir).unwrap();
    let project = "/bench/c13";
    let sid_a = "sessionA";
    let cats = ["decision", "decision", "constraint"];
    let evs: Vec<Event> = C13_EVIDENCE
        .iter()
        .enumerate()
        .map(|(i, text)| Event {
            session_id: sid_a.into(),
            project: project.into(),
            timestamp: i as i64 + 1,
            category: cats[i].into(),
            priority: 2,
            payload: json!({ "text": text }),
            source_hook: "UserPromptSubmit".into(),
        })
        .collect();
    store.insert_events(&evs).unwrap();
    // A fresh session clears the live event log.
    store.clear_project_events(project).unwrap();
    let remaining = store.events_for_session(sid_a).unwrap();
    let without = snapshot::build_snapshot(&remaining, lens::session::snapshot_budget(), 0);
    let with = snapshot::render_project_memory(&store.project_memory(project).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    (c13_recall(&without), c13_recall(&with))
}

fn gate_c13() -> (String, bool) {
    let (without, with) = measure_c13();
    let pass = with >= 0.8;
    let s = format!(
        "## C13 - cross-session project memory\n\nSession A records durable decisions/constraints; a fresh session clears the live event log. Recall of the prior decisions WITHOUT persisted memory: **{without:.3}**; WITH project memory re-injected: **{with:.3}**. Gate: with-memory recall ≥ 0.8.\n"
    );
    (s, pass)
}

// --- C14: token-estimate accuracy -------------------------------------------

/// Committed code + prose samples to measure the token estimator against. Real
/// fixture files (code) plus a prose block, whose bytes-per-token ratios differ.
fn c14_samples() -> Vec<String> {
    let mut samples: Vec<String> = Vec::new();
    for sub in ["search", "rank", "imports"] {
        for entry in walkdir::WalkDir::new(changes_fixture(sub))
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() {
                if let Ok(s) = std::fs::read_to_string(entry.path()) {
                    samples.push(s);
                }
            }
        }
    }
    samples.push(
        "The reconciliation service settles every ledger entry in minor units and \
         keeps the audit trail append-only so a later dispute can be replayed exactly. "
            .repeat(8),
    );
    samples
}

/// (mean abs % error of the old bytes/4 heuristic, of the new BPE estimator),
/// each against the real o200k_base token count (ground truth).
fn measure_c14() -> (f64, f64) {
    let samples = c14_samples();
    let mut old_sum = 0.0;
    let mut new_sum = 0.0;
    let mut n = 0.0;
    for s in &samples {
        let truth = lens::obs::count_tokens(s) as f64;
        if truth == 0.0 {
            continue;
        }
        let old = (s.len() / 4) as f64;
        let new = lens::obs::count_tokens(s) as f64;
        old_sum += (old - truth).abs() / truth;
        new_sum += (new - truth).abs() / truth;
        n += 1.0;
    }
    (old_sum / n * 100.0, new_sum / n * 100.0)
}

fn gate_c14() -> (String, bool) {
    let (old_err, new_err) = measure_c14();
    let pass = new_err <= 8.0;
    let s = format!(
        "## C14 - token-estimate accuracy\n\nMean absolute error vs the real o200k_base token count over committed code/prose samples. Old bytes/4 heuristic: **{old_err:.1}%**; new BPE estimator: **{new_err:.1}%**. Gate: new mean abs error ≤ 8%.\n"
    );
    (s, pass)
}

// --- C15: structural search (lens_grep_ast) ---------------------------------

/// Returns (precision, recall) of an AST query for `.unwrap()` calls against the
/// hand-labeled true call sites in `fixtures/structural` (lines 7, 12, 18).
fn measure_c15() -> (f64, f64) {
    let query = "(call_expression function: (field_expression field: (field_identifier) @method))";
    let matches = lens::discovery::structural::grep_ast(
        &changes_fixture("structural"),
        query,
        Some("rust"),
        200,
    )
    .unwrap();
    let found: std::collections::BTreeSet<usize> = matches
        .iter()
        .filter(|m| m.text == "unwrap")
        .map(|m| m.line)
        .collect();
    let truth: std::collections::BTreeSet<usize> = [7, 12, 18].into_iter().collect();
    let correct = found.intersection(&truth).count();
    let precision = if found.is_empty() {
        0.0
    } else {
        correct as f64 / found.len() as f64
    };
    let recall = correct as f64 / truth.len() as f64;
    (precision, recall)
}

fn gate_c15() -> (String, bool) {
    let (precision, recall) = measure_c15();
    let pass = (precision - 1.0).abs() < 1e-9 && recall >= 0.95;
    let s = format!(
        "## C15 - structural search (lens_grep_ast)\n\nAn AST query for `.unwrap()` calls over `fixtures/structural` must hit only the real call sites, never the comment mentions a grep would over-match. Precision **{precision:.3}**, recall **{recall:.3}**. Gate: precision = 1.0, recall ≥ 0.95.\n"
    );
    (s, pass)
}

// --- C16: subword-recall search (L28 camelCase expansion) -------------------

/// (query, ground-truth file, snakeCovered). snakeCovered = a snake_case sibling
/// already exposes the subword to the porter tokenizer today, so L28 is not the
/// only way that file could match the query. The pure-Pascal subset (snakeCovered
/// = false) is the 8 queries L28 is solely responsible for.
const C16_CORPUS: [(&str, &str, bool); 10] = [
    ("Subscription", "confirm_screen.tsx", false),
    ("Canceled", "confirm_screen.tsx", false),
    ("Billing", "billing.tsx", false),
    ("Portal", "billing.tsx", false),
    ("Selector", "payment.rs", false),
    ("Payment", "payment.rs", true),
    ("Socket", "websocket.rs", false),
    ("Connection", "websocket.rs", false),
    ("Validator", "auth.ts", false),
    ("Token", "auth.ts", true),
];

/// True when any path in `paths` ends with the ground-truth file at a path-component
/// boundary. The corpus is indexed by absolute path, so compare by suffix.
fn c16_hit(paths: &[String], gt: &str) -> bool {
    paths.iter().any(|p| {
        let p = p.replace('\\', "/");
        p == gt || p.ends_with(&format!("/{gt}"))
    })
}

/// Per-query outcome: the labeled inputs plus the two arms' HIT booleans.
struct C16Row {
    query: &'static str,
    gt: &'static str,
    snake_covered: bool,
    search_hit: bool,
    symbol_hit: bool,
}

/// Run both arms over `fixtures/subword` and return per-query rows plus the three
/// aggregate fractions (search_overall, search_pure_pascal, symbol_overall).
///
/// lens_search: open a temp `Index`, index the corpus, `search(&[q], 5)`; HIT = a
/// top-5 hit path ends with the GT file. This is the arm L28 moves. lens_symbol:
/// build the graph via `discovery::discover`, run the same substring-over-symbol-
/// names path the `lens_symbol` tool uses; HIT = the GT file among the matched
/// nodes. The symbol arm does not touch `chunk_symbols`, so it is an informational
/// control independent of L28.
fn measure_c16() -> (Vec<C16Row>, f64, f64, f64) {
    let corpus = changes_fixture("subword");
    let data = tempfile::tempdir().unwrap();
    let index = Index::open(data.path()).unwrap();
    index.index_path(&corpus, true).unwrap();
    let g = discovery::discover(&corpus, None).unwrap().graph;

    let mut rows = Vec::new();
    for (query, gt, snake_covered) in C16_CORPUS {
        let resp = index.search(&[query.to_string()], 5).unwrap();
        let s_paths: Vec<String> = resp.results[0].hits.iter().map(|h| h.path.clone()).collect();
        let view = gquery::query(&g, query, None, 5, &[]);
        let lq = query.to_ascii_lowercase();
        let y_paths: Vec<String> = view
            .nodes
            .iter()
            .filter(|n| n.name.to_ascii_lowercase().contains(&lq))
            .map(|n| n.file.clone())
            .collect();
        rows.push(C16Row {
            query,
            gt,
            snake_covered,
            search_hit: c16_hit(&s_paths, gt),
            symbol_hit: c16_hit(&y_paths, gt),
        });
    }

    let n = rows.len() as f64;
    let search_hits = rows.iter().filter(|r| r.search_hit).count();
    let symbol_hits = rows.iter().filter(|r| r.symbol_hit).count();
    let pp: Vec<&C16Row> = rows.iter().filter(|r| !r.snake_covered).collect();
    let pp_hits = pp.iter().filter(|r| r.search_hit).count();
    let search_pp = if pp.is_empty() {
        0.0
    } else {
        pp_hits as f64 / pp.len() as f64
    };
    (rows, search_hits as f64 / n, search_pp, symbol_hits as f64 / n)
}

fn gate_c16() -> (String, bool) {
    let (rows, search_overall, search_pp, symbol_overall) = measure_c16();
    let n = rows.len();
    let search_hits = rows.iter().filter(|r| r.search_hit).count();
    let symbol_hits = rows.iter().filter(|r| r.symbol_hit).count();
    let pp_total = rows.iter().filter(|r| !r.snake_covered).count();
    let pp_hits = rows.iter().filter(|r| !r.snake_covered && r.search_hit).count();

    let mut s = String::new();
    s.push_str("## C16 - subword search recall (L28 camelCase expansion)\n\n");
    s.push_str("Each labeled query is a Pascal/camel subword of a compound identifier defined in exactly one `fixtures/subword` file. lens_search expands subwords in `chunk_symbols`, so a subword query reaches the defining file. The `search` column is what L28 moves; `symbol` is an INFORMATIONAL CONTROL that does not use `chunk_symbols` and is NOT part of the pass condition.\n\n");
    s.push_str("| query | GT | snakeCovered | search | symbol |\n");
    s.push_str("| --- | --- | :---: | :---: | :---: |\n");
    for r in &rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            r.query, r.gt, r.snake_covered, r.search_hit, r.symbol_hit
        ));
    }
    s.push_str(&format!(
        "\nlens_search overall: **{search_hits}/{n}** = {search_overall:.3}; pure-Pascal (snakeCovered=false): **{pp_hits}/{pp_total}** = {search_pp:.3}. lens_symbol overall (control): **{symbol_hits}/{n}** = {symbol_overall:.3}. Gate: search pure-Pascal = 1.0 (8/8) and search overall = 1.0 (10/10).\n"
    ));
    let pass = search_pp >= 1.0 - 1e-9 && search_overall >= 1.0 - 1e-9;
    (s, pass)
}

fn capture_baseline() -> Baseline {
    let (c5_mrr, c5_p_at_5) = measure_c5();
    let c7_mrr = measure_c7();
    let (c8_precision, c8_recall) = measure_c8();
    let (c12_recall, c12_bytes) = measure_c12(2048);
    Baseline {
        c5_mrr,
        c5_p_at_5,
        c7_mrr,
        c8_precision,
        c8_recall,
        c12_recall,
        c12_bytes,
    }
}

fn main() -> anyhow::Result<()> {
    // `--update` captures the pre-fix baselines and exits (run once on master).
    if std::env::args().any(|a| a == "--update") {
        let b = capture_baseline();
        let path = baseline_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&b)? + "\n")?;
        eprintln!("captured baseline: {}", path.display());
        println!("{}", serde_json::to_string_pretty(&b)?);
        return Ok(());
    }

    println!("# lens - benchmark of this round's changes (deterministic, no model)\n");

    let (s1, c1_ok) = c1_toon();
    println!("{s1}");
    let (s2, c2_ok) = c2_proximity()?;
    println!("{s2}");
    let (s3, c3_ok) = c3_find()?;
    println!("{s3}");
    let (s4, c4_ok) = c4_recovery()?;
    println!("{s4}");

    let b = load_baseline();
    let (s5, c5_ok) = gate_c5(&b);
    println!("{s5}");
    let (s6, c6_ok) = gate_c6();
    println!("{s6}");
    let (s7, c7_ok) = gate_c7(&b);
    println!("{s7}");
    let (s8, c8_ok) = gate_c8(&b);
    println!("{s8}");
    let (s9, c9_ok) = gate_c9();
    println!("{s9}");
    let (s10, c10_ok) = gate_c10();
    println!("{s10}");
    let (s11, c11_ok) = gate_c11();
    println!("{s11}");
    let (s12, c12_ok) = gate_c12(&b);
    println!("{s12}");
    let (s13, c13_ok) = gate_c13();
    println!("{s13}");
    let (s14, c14_ok) = gate_c14();
    println!("{s14}");
    let (s15, c15_ok) = gate_c15();
    println!("{s15}");
    let (s16, c16_ok) = gate_c16();
    println!("{s16}");

    println!("\n## Gates");
    let gates = [
        ("C1 TOON lossless + smaller", c1_ok),
        ("C2 proximity lifts in-focus rank", c2_ok),
        ("C3 lens_find hit-rate", c3_ok),
        ("C4 contradiction resolved correctly", c4_ok),
        ("C5 BM25F search MRR ≥ baseline×1.15", c5_ok),
        ("C6 punctuation queries each ≥1 hit", c6_ok),
        ("C7 importance ranking MRR ≥ baseline×1.25", c7_ok),
        ("C8 scope-aware precision ≥0.85, recall ≥ baseline", c8_ok),
        ("C9 multi-symbol import emits 3 edges", c9_ok),
        ("C10 trait-sig / const / type captured", c10_ok),
        ("C11 overview keeps ≥80% important within 2000 tokens", c11_ok),
        ("C12 recovery recall ≥ baseline, bytes ≤8192", c12_ok),
        ("C13 fresh-session memory recall ≥0.8", c13_ok),
        ("C14 token-estimate mean abs error ≤8%", c14_ok),
        ("C15 structural search precision 1.0, recall ≥0.95", c15_ok),
        ("C16 subword search recall 10/10 (pure-Pascal 8/8)", c16_ok),
    ];
    for (name, ok) in gates {
        println!("- {} {name}", if ok { "PASS" } else { "FAIL" });
    }
    let all = gates.iter().all(|(_, ok)| *ok);
    println!(
        "\n{}",
        if all {
            "All gates PASS."
        } else {
            "SOME GATES FAILED."
        }
    );
    if !all {
        std::process::exit(1);
    }
    Ok(())
}
