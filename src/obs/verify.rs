//! `lens verify` — the reversibility audit, runnable on real session data.
//!
//! The store already guarantees losslessness; this lets you *see* it hold:
//!
//! - `verify <ref>` re-fetches and prints the full blob a compact result stood for.
//! - `verify --op <n>` shows, for op #n in ops.log, the compact form, the full form,
//!   and that the recorded bytes/ref are consistent.
//! - `verify --roundtrip <ref>` checks content-addressed integrity plus (for JSON)
//!   that decompaction reproduces the original, printed PASS/FAIL.
//! - `verify --all-recent <n>` roundtrip-checks the last N store-backed ops.

use anyhow::Result;
use serde_json::Value;

use super::{data_dir, stats::read_records};
use crate::store::compress::{compact_json, drop_nulls, expand_json};
use crate::store::Store;

/// CLI entry: `args` is everything after `verify`.
pub fn run_cli(args: &[String]) -> Result<()> {
    let dir = data_dir();
    let store = Store::open(&dir)?;

    match args.first().map(|s| s.as_str()) {
        Some("--roundtrip") => {
            let reference = arg(args, 1, "--roundtrip <store_ref>")?;
            let r = roundtrip(&store, &reference);
            println!("{}", r.line(&reference));
            std::process::exit(if r.pass { 0 } else { 1 });
        }
        Some("--all-recent") => {
            let n: usize = arg(args, 1, "--all-recent <n>")?
                .parse()
                .map_err(|_| anyhow::anyhow!("--all-recent expects a number"))?;
            all_recent(&store, &dir, n);
        }
        Some("--op") => {
            let n: usize = arg(args, 1, "--op <n>")?
                .parse()
                .map_err(|_| anyhow::anyhow!("--op expects an op number (1-based)"))?;
            show_op(&store, &dir, n)?;
        }
        Some(reference) if !reference.starts_with("--") => match store.get(reference)? {
            Some(content) => println!("{content}"),
            None => {
                eprintln!("unknown ref '{reference}'");
                std::process::exit(1);
            }
        },
        _ => {
            eprintln!("usage: lens verify <ref> | --op <n> | --roundtrip <ref> | --all-recent <n>");
            std::process::exit(2);
        }
    }
    Ok(())
}

fn arg(args: &[String], i: usize, usage: &str) -> Result<String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("missing argument: {usage}"))
}

/// Outcome of a single roundtrip check.
struct Roundtrip {
    pass: bool,
    integrity: bool,
    /// `None` when the blob is not JSON (no compaction transform to invert).
    compaction: Option<bool>,
}

impl Roundtrip {
    fn line(&self, reference: &str) -> String {
        let verdict = if self.pass { "PASS" } else { "FAIL" };
        let comp = match self.compaction {
            Some(true) => "ok",
            Some(false) => "MISMATCH",
            None => "n/a (verbatim blob)",
        };
        format!(
            "{verdict} {reference}  (integrity={}, compaction_roundtrip={comp})",
            if self.integrity { "ok" } else { "CORRUPT" }
        )
    }
}

/// Check a store ref two ways:
///
/// 1. content-addressed integrity — blake3(content) must equal the ref;
/// 2. for JSON blobs, decompaction (`expand(compact(v))`) reproduces the original
///    (mod dropped nulls), which is the lossless compaction contract.
///
/// A blob that fails either way is a real bug surfaced, not a rubber stamp.
fn roundtrip(store: &Store, reference: &str) -> Roundtrip {
    let content = match store.get(reference) {
        Ok(Some(c)) => c,
        _ => {
            return Roundtrip {
                pass: false,
                integrity: false,
                compaction: None,
            }
        }
    };
    let integrity = blake3::hash(content.as_bytes()).to_hex().to_string() == reference;
    let compaction = match serde_json::from_str::<Value>(&content) {
        Ok(v) if v.is_object() || v.is_array() => {
            Some(expand_json(&compact_json(&v)) == drop_nulls(&v))
        }
        _ => None,
    };
    let pass = integrity && compaction.unwrap_or(true);
    Roundtrip {
        pass,
        integrity,
        compaction,
    }
}

fn all_recent(store: &Store, dir: &std::path::Path, n: usize) {
    let records = read_records(dir, None);
    let backed: Vec<&str> = records
        .iter()
        .filter_map(|r| r.store_ref.as_deref())
        .collect();
    let recent: Vec<&str> = backed.iter().rev().take(n).copied().collect();
    if recent.is_empty() {
        println!("no store-backed ops found in ops.log");
        return;
    }
    let mut fails = 0;
    for reference in &recent {
        let r = roundtrip(store, reference);
        if !r.pass {
            fails += 1;
        }
        println!("{}", r.line(reference));
    }
    println!(
        "checked {} store-backed op(s): {} PASS, {} FAIL",
        recent.len(),
        recent.len() - fails,
        fails
    );
    std::process::exit(if fails > 0 { 1 } else { 0 });
}

fn show_op(store: &Store, dir: &std::path::Path, n: usize) -> Result<()> {
    let records = read_records(dir, None);
    let rec = records
        .get(n.wrapping_sub(1))
        .ok_or_else(|| anyhow::anyhow!("op #{n} not found ({} ops in log)", records.len()))?;
    println!(
        "op #{n}: tool={} ts={} outcome={}",
        rec.tool, rec.ts, rec.outcome
    );
    println!(
        "  recorded: raw_bytes_in={} bytes_returned={} tokens_saved_est={}",
        rec.raw_bytes_in, rec.bytes_returned, rec.tokens_saved_est
    );
    match &rec.store_ref {
        None => {
            println!("  no store_ref — nothing was offloaded for this op");
        }
        Some(reference) => {
            println!("  store_ref={reference}");
            match store.get(reference)? {
                None => println!("  !! store has no blob for this ref (inconsistent)"),
                Some(full) => {
                    // The compact form the agent saw is deterministic from the
                    // stored original, so we can reproduce it for inspection.
                    if let Ok(v) = serde_json::from_str::<Value>(&full) {
                        let compact = compact_json(&v);
                        println!("  --- compact form the agent saw ---");
                        println!("  {compact}");
                    }
                    println!("  --- full form (from store) ---");
                    println!("{full}");
                    let consistent = full.len() as u64 >= rec.bytes_returned;
                    println!(
                        "  consistency: store holds {} bytes, recoverable=yes, recorded bytes_returned={} ({})",
                        full.len(),
                        rec.bytes_returned,
                        if consistent { "consistent" } else { "SUSPECT" }
                    );
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_passes_on_real_compaction_blob() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        // A graph-shaped payload, exactly what maybe_compact offloads.
        let original = json!({
            "nodes": [
                {"file": "a.rs", "kind": "function", "line": 1, "name": "alpha"},
                {"file": "a.rs", "kind": "function", "line": 2, "name": "beta"},
            ],
            "edges": [{"from": "alpha", "kind": "calls", "to": "beta"}]
        });
        let reference = store.put(&original.to_string()).unwrap();
        let r = roundtrip(&store, &reference);
        assert!(r.pass, "{}", r.line(&reference));
        assert!(r.integrity);
        assert_eq!(r.compaction, Some(true));
    }

    #[test]
    fn corrupted_store_entry_fails_roundtrip() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let reference = store.put("{\"nodes\":[],\"edges\":[]}").unwrap();
        // Deliberately corrupt the blob content so it no longer matches the hash.
        let conn = rusqlite::Connection::open(dir.path().join("store.db")).unwrap();
        conn.execute(
            "UPDATE blobs SET content = ?1 WHERE hash = ?2",
            rusqlite::params![b"tampered".as_slice(), reference],
        )
        .unwrap();
        let r = roundtrip(&store, &reference);
        assert!(!r.pass, "corruption must fail the audit");
        assert!(!r.integrity);
    }

    #[test]
    fn unknown_ref_fails() {
        let dir = tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        let r = roundtrip(&store, "deadbeef");
        assert!(!r.pass);
    }
}
