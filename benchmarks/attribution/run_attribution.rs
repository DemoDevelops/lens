//! Deterministic replay-validation harness for darkroom savings attribution.
//!
//! Pure data join, **no LLM/model call anywhere**: reads the live `ops.log`,
//! joins each darkroom/skeleton op with a `session_id` against the Claude
//! session transcript it came from, and checks whether the write-time credit
//! classifier (`lens::obs::credit`) agrees with what that transcript actually
//! shows (paths the agent Read/Grep/Glob'd in-session, and whether the
//! routing deny fired). Prints the honest-vs-current split and exits non-zero
//! if the calibration gate is breached.
//!
//!   cargo run --release --bin bench_attribution [-- --ledger <path/to/ops.log>]
//!
//! Read-only: this binary never constructs an `OpLog`/`OpHandle`, so it can
//! never append a record. `.cargo/config.toml` also sets
//! `LENS_NO_GLOBAL_MIRROR=1` for everything cargo launches, as a second
//! guard against mirroring a fixture op into the real `~/.lens` ledger.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use lens::obs::credit::{self, CreditClass};
use lens::obs::stats;

/// Prefix of `routing::READ_DENY_REASON` (`src/routing/mod.rs`), stable even
/// if the tip text is re-worded later; a plain substring search over the raw
/// transcript is enough to know the deny fired somewhere in the session.
const DENY_MARKER: &str = "Too many consecutive Read/Grep calls";

fn main() {
    let dir = ledger_dir(parse_ledger_arg());
    let records = stats::read_records(&dir, None);
    if records.is_empty() {
        eprintln!(
            "bench_attribution: no op records found under {} (pass --ledger <path to ops.log>)",
            dir.display()
        );
        std::process::exit(1);
    }

    let transcripts = index_transcripts();
    let mut evidence_cache: HashMap<String, Option<SessionEvidence>> = HashMap::new();

    let mut current_total: i64 = 0;
    let mut honest_total: i64 = 0;
    let mut class_agg: BTreeMap<&'static str, ClassAgg> = BTreeMap::new();

    let mut path_bearing_total: u64 = 0;
    let mut ops_no_session: u64 = 0;
    let mut ops_no_transcript: u64 = 0;
    let mut ops_joined: u64 = 0;

    let mut label_read: u64 = 0;
    let mut label_deny: u64 = 0;
    let mut label_volunteered_evidence: u64 = 0;

    let mut read_in_session: u64 = 0;
    let mut read_in_session_contextbound: u64 = 0;
    let mut volunteered_total: u64 = 0;
    let mut volunteered_never_read: u64 = 0;

    for r in &records {
        if r.tool != "lens_run_file" && r.tool != "lens_skeleton" {
            continue;
        }
        let Some(path) = r.input_summary.get("path").and_then(|p| p.as_str()) else {
            continue;
        };
        path_bearing_total += 1;

        let class = credit::classify(&r.tool, &r.input_summary, r.raw_bytes_in);
        let credited = credit::credited_raw(class, r.raw_bytes_in);
        let raw_delta = (r.raw_bytes_in as i64 - r.bytes_returned as i64).max(0);
        let credited_delta = (credited as i64 - r.bytes_returned as i64).max(0);
        current_total += raw_delta / 4;
        honest_total += credited_delta / 4;

        let agg = class_agg.entry(class.as_str()).or_default();
        agg.count += 1;
        agg.raw += r.raw_bytes_in;
        agg.credited += credited;

        let Some(session_id) = r.session_id.as_deref() else {
            ops_no_session += 1;
            continue;
        };
        let evidence = evidence_cache
            .entry(session_id.to_string())
            .or_insert_with(|| transcripts.get(session_id).map(|p| parse_transcript(p)));
        let Some(ev) = evidence else {
            ops_no_transcript += 1;
            continue;
        };
        ops_joined += 1;

        let was_read = paths_match(path, &ev.paths);
        if was_read {
            label_read += 1;
            read_in_session += 1;
            if class == CreditClass::ContextBound {
                read_in_session_contextbound += 1;
            }
        } else if ev.deny {
            label_deny += 1;
        } else {
            label_volunteered_evidence += 1;
        }

        if class == CreditClass::Volunteered {
            volunteered_total += 1;
            if !was_read {
                volunteered_never_read += 1;
            }
        }
    }

    let read_agree = if read_in_session == 0 {
        1.0
    } else {
        read_in_session_contextbound as f64 / read_in_session as f64
    };
    let vol_never_read_frac = if volunteered_total == 0 {
        1.0
    } else {
        volunteered_never_read as f64 / volunteered_total as f64
    };
    let agreement = read_agree.min(vol_never_read_frac);
    let honest_pct = if current_total == 0 {
        0.0
    } else {
        honest_total as f64 / current_total as f64
    };

    println!("=== bench_attribution: darkroom savings calibration ===");
    println!("ledger: {}", dir.display());
    println!("path-bearing ops (lens_run_file/lens_skeleton with a path): {path_bearing_total}");
    println!();
    println!("current total credit (raw byte-delta/4):       {current_total} tok");
    println!("honest total credit (classified byte-delta/4):  {honest_total} tok");
    println!("honest % of current:                             {:.1}%", honest_pct * 100.0);
    println!();
    println!("-- CreditClass split (path-bearing ops) --");
    for (name, agg) in &class_agg {
        println!(
            "  {name:>13}: {:>5} ops | raw={:>12} bytes | credited={:>12} bytes",
            agg.count, agg.raw, agg.credited
        );
    }
    println!();
    println!(
        "-- transcript-join labels (of {ops_joined} joined ops; {ops_no_session} no session_id, {ops_no_transcript} session but no findable transcript) --"
    );
    println!("  read_in_session:      {label_read}");
    println!("  deny_in_session:      {label_deny}");
    println!("  volunteered_evidence: {label_volunteered_evidence}");
    println!();
    println!("-- classifier-vs-transcript agreement --");
    println!(
        "  read_in_session -> classifier ContextBound: {read_in_session_contextbound}/{read_in_session} = {read_agree:.3}"
    );
    println!(
        "  classifier Volunteered -> never Read in-session: {volunteered_never_read}/{volunteered_total} = {vol_never_read_frac:.3}"
    );
    println!("  overall agreement (min of the two):          {agreement:.3}");
    println!();
    println!(
        "-- coverage --\n  transcripts found: {ops_joined} / {} session-bearing path-bearing ops ({ops_no_transcript} missing, {ops_no_session} had no session_id at all)",
        ops_joined + ops_no_transcript
    );
    println!();

    let honest_ok = (0.35..=0.55).contains(&honest_pct);
    let agreement_ok = agreement >= 0.9;
    if honest_ok && agreement_ok {
        println!(
            "PASS: honest%={:.1}% in [35,55], agreement={:.3} >= 0.9",
            honest_pct * 100.0,
            agreement
        );
        std::process::exit(0);
    }
    println!(
        "FAIL: honest%={:.1}% (want [35,55]) honest_ok={honest_ok}, agreement={:.3} (want >=0.9) agreement_ok={agreement_ok}",
        honest_pct * 100.0,
        agreement
    );
    std::process::exit(1);
}

#[derive(Default)]
struct ClassAgg {
    count: u64,
    raw: u64,
    credited: u64,
}

/// Evidence pulled from one Claude session transcript: the raw path strings
/// seen in `Read`/`Grep`/`Glob` tool-call inputs, and whether the routing
/// deny (`READ_DENY_REASON`) fired anywhere in the session.
struct SessionEvidence {
    paths: HashSet<String>,
    deny: bool,
}

/// First positional (non-`--flag`) CLI arg, or the value following
/// `--ledger`/`--ledger=<path>`. Accepts either the `ops.log` file itself or
/// its containing directory.
fn parse_ledger_arg() -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if let Some(v) = a.strip_prefix("--ledger=") {
            return Some(v.to_string());
        }
        if a == "--ledger" {
            return args.next();
        }
        if !a.starts_with("--") {
            return Some(a);
        }
    }
    None
}

/// Resolve the CLI arg (or the `.lens/ops.log` default, relative to cwd) to
/// the directory `stats::read_records` expects (it appends `ops.log` itself).
fn ledger_dir(arg: Option<String>) -> PathBuf {
    let raw = arg.unwrap_or_else(|| ".lens/ops.log".to_string());
    let path = PathBuf::from(raw);
    if path.file_name().and_then(|f| f.to_str()) == Some("ops.log") {
        path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
    } else {
        path
    }
}

/// One-time scan of `~/.claude*/projects/*lens*/*.jsonl`, indexed by session
/// id (the file stem), so each unique session transcript is located once
/// regardless of how many ops in the ledger reference it.
fn index_transcripts() -> HashMap<String, PathBuf> {
    let mut idx = HashMap::new();
    let Some(home) = std::env::var_os("HOME") else {
        return idx;
    };
    let Ok(home_entries) = std::fs::read_dir(&home) else {
        return idx;
    };
    for e in home_entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(".claude") {
            continue;
        }
        let projects_dir = e.path().join("projects");
        let Ok(proj_entries) = std::fs::read_dir(&projects_dir) else {
            continue;
        };
        for p in proj_entries.flatten() {
            let pname = p.file_name().to_string_lossy().to_lowercase();
            if !pname.contains("lens") {
                continue;
            }
            let Ok(files) = std::fs::read_dir(p.path()) else {
                continue;
            };
            for f in files.flatten() {
                let path = f.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    idx.entry(stem.to_string()).or_insert(path);
                }
            }
        }
    }
    idx
}

/// Parse a transcript: every `Read`/`Grep`/`Glob` tool-call path, plus
/// whether `READ_DENY_REASON` fired anywhere. Skips lines that aren't valid
/// JSON (transcripts are JSONL; a truncated last line is tolerated).
fn parse_transcript(path: &Path) -> SessionEvidence {
    let mut paths = HashSet::new();
    let mut deny = false;
    let Ok(text) = std::fs::read_to_string(path) else {
        return SessionEvidence { paths, deny };
    };
    if text.contains(DENY_MARKER) {
        deny = true;
    }
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let input = block.get("input");
            let field = match name {
                "Read" => "file_path",
                "Grep" | "Glob" => "path",
                _ => continue,
            };
            if let Some(p) = input.and_then(|i| i.get(field)).and_then(|p| p.as_str()) {
                paths.insert(p.to_string());
            }
        }
    }
    SessionEvidence { paths, deny }
}

/// Undo the literal `\ ` (backslash-escaped space) some transcripts carry in
/// shell-quoted paths, so string comparison isn't defeated by it.
fn normalize(raw: &str) -> String {
    raw.replace("\\ ", " ")
}

/// Whether `op_path` (from an `OpRecord`'s `input_summary.path`) was among
/// the paths a session's transcript shows as Read/Grep/Glob'd. Matches by
/// exact (normalized) path, by path-component suffix (handles differing
/// absolute prefixes / relative vs. absolute), and by basename as a
/// last-resort fallback.
fn paths_match(op_path: &str, evidence_paths: &HashSet<String>) -> bool {
    let op_norm = normalize(op_path);
    let op_pb = Path::new(&op_norm);
    let op_base = op_pb.file_name();
    evidence_paths.iter().any(|raw| {
        let sp_norm = normalize(raw);
        let sp_pb = Path::new(&sp_norm);
        if sp_pb == op_pb {
            return true;
        }
        if sp_pb.ends_with(op_pb) || op_pb.ends_with(sp_pb) {
            return true;
        }
        matches!((op_base, sp_pb.file_name()), (Some(a), Some(b)) if a == b)
    })
}
