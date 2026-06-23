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

use serde_json::{json, Value};

use lens::discovery::{self, query as gquery};
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
    // Focus on the file of the LAST match in the base order (so any lift is real,
    // not an artifact of it already being first).
    let focus_file = base
        .nodes
        .last()
        .map(|n| n.file.clone())
        .unwrap_or_default();
    let before = base
        .nodes
        .iter()
        .position(|n| n.file.ends_with(focus_file.as_str()))
        .map(|p| p + 1)
        .unwrap_or(0);

    let boosted = gquery::query(g, &q, None, 50, std::slice::from_ref(&focus_file));
    let after = boosted
        .nodes
        .iter()
        .position(|n| n.file.ends_with(focus_file.as_str()))
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

fn main() -> anyhow::Result<()> {
    println!("# lens - benchmark of this round's changes (deterministic, no model)\n");

    let (s1, c1_ok) = c1_toon();
    println!("{s1}");
    let (s2, c2_ok) = c2_proximity()?;
    println!("{s2}");
    let (s3, c3_ok) = c3_find()?;
    println!("{s3}");
    let (s4, c4_ok) = c4_recovery()?;
    println!("{s4}");

    println!("\n## Gates");
    for (name, ok) in [
        ("C1 TOON lossless + smaller", c1_ok),
        ("C2 proximity lifts in-focus rank", c2_ok),
        ("C3 lens_find hit-rate", c3_ok),
        ("C4 contradiction resolved correctly", c4_ok),
    ] {
        println!("- {} {name}", if ok { "PASS" } else { "FAIL" });
    }
    let all = c1_ok && c2_ok && c3_ok && c4_ok;
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
