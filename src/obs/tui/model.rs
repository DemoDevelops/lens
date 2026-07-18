//! Snapshot data extraction shared by every panel: the `Value` -> number/series
//! helpers ported from the old string renderer, plus one accessor per
//! `stats::SNAPSHOT_DIMENSIONS` key — this file textually references every
//! dimension, which is what the parity tripwire in `dashboard.rs` scans for.

use serde_json::Value;

pub(crate) use crate::obs::stats::{human_bytes, human_count};

// ---------------------------------------------------------------------------
// One accessor per snapshot dimension (the parity-tripwire references)
// ---------------------------------------------------------------------------

/// `by_tool`: the per-tool adoption rows.
pub(crate) fn by_tool(s: &Value) -> &Value {
    &s["by_tool"]
}
/// `by_mechanism`: per-mechanism ops/saved chips.
pub(crate) fn by_mechanism(s: &Value) -> &Value {
    &s["by_mechanism"]
}
/// `applied_value`: benchmark per-op rates applied to this scope's live ops.
pub(crate) fn applied_value(s: &Value) -> &Value {
    &s["applied_value"]
}
/// `actual_usage`: real per-model spend/mix from Claude Code's own transcripts.
pub(crate) fn actual_usage(s: &Value) -> &Value {
    &s["actual_usage"]
}
/// `rtk`: RTK's measured shell-savings plane.
pub(crate) fn rtk(s: &Value) -> &Value {
    &s["rtk"]
}
/// `activity`: session activity (events, sessions, categories).
pub(crate) fn activity(s: &Value) -> &Value {
    &s["activity"]
}
/// `store_size`: reversible-store bytes on disk (footer).
pub(crate) fn store_size(s: &Value) -> u64 {
    getu(s, "store_size")
}

// ---------------------------------------------------------------------------
// Small value helpers (ported verbatim from the string renderer)
// ---------------------------------------------------------------------------

pub(crate) fn geti(snap: &Value, k: &str) -> i64 {
    snap.get(k).and_then(|v| v.as_i64()).unwrap_or(0)
}
pub(crate) fn getu(snap: &Value, k: &str) -> u64 {
    snap.get(k).and_then(|v| v.as_u64()).unwrap_or(0)
}
pub(crate) fn saved_mcp(snap: &Value) -> u64 {
    snap.get("tokens_saved_mcp")
        .and_then(|v| v.as_i64())
        .or_else(|| snap.get("tokens_saved_est").and_then(|v| v.as_i64()))
        .unwrap_or(0)
        .max(0) as u64
}
/// The measured floor: bytes provably kept out of context via store-offload,
/// converted to tokens the same way `tokens_saved_est` is — a hard fact, not a
/// classified estimate.
pub(crate) fn saved_measured_floor(snap: &Value) -> u64 {
    snap.get("tokens_saved_measured_floor")
        .and_then(|v| v.as_i64())
        .unwrap_or(0)
        .max(0) as u64
}
pub(crate) fn buckets(snap: &Value, key: &str) -> Vec<i64> {
    snap.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().map(|x| x.as_i64().unwrap_or(0)).collect())
        .unwrap_or_default()
}

/// US dollars at 2/3/4 significant decimals depending on magnitude, so a sub-cent
/// figure doesn't round away to `$0.00`. Mirrors the web `money()` formatter.
pub(crate) fn money(dollars: f64) -> String {
    // Normalize IEEE -0.0 to 0.0: Rust's formatter keeps the sign ("$-0.0000")
    // where the web's `toFixed` drops it ("$0.0000"), so a zero headline reads clean.
    let dollars = if dollars == 0.0 { 0.0 } else { dollars };
    if dollars >= 1.0 {
        format!("${dollars:.2}")
    } else if dollars >= 0.01 {
        format!("${dollars:.3}")
    } else {
        format!("${dollars:.4}")
    }
}

/// Human display name for a canonical model key, mirroring the web `modelLabel()`
/// so the two don't drift. Falls back to the raw id minus the `claude-` prefix and
/// any trailing `-DDDDDDDD` dated-snapshot suffix (`raw.replace(/^claude-/,
/// '').replace(/-\d{8}$/, '')` in JS).
pub(crate) fn model_label(model: &str) -> String {
    match model {
        "claude-opus-4-8" => "Opus 4.8".to_string(),
        "claude-sonnet-5" => "Sonnet 5".to_string(),
        "claude-haiku-4-5" => "Haiku 4.5".to_string(),
        "claude-fable-5" => "Fable 5".to_string(),
        other => {
            let stripped = other.strip_prefix("claude-").unwrap_or(other);
            match stripped.rfind('-') {
                Some(i)
                    if stripped[i + 1..].len() == 8
                        && stripped[i + 1..].bytes().all(|b| b.is_ascii_digit()) =>
                {
                    stripped[..i].to_string()
                }
                _ => stripped.to_string(),
            }
        }
    }
}

/// Seconds as a compact human duration: `~Ns` / `~N.N min` / `~N.N h`.
pub(crate) fn human_time(secs: f64) -> String {
    if secs < 90.0 {
        format!("~{secs:.0}s")
    } else if secs < 5400.0 {
        format!("~{:.1} min", secs / 60.0)
    } else {
        format!("~{:.1} h", secs / 3600.0)
    }
}

// ---------------------------------------------------------------------------
// Bucket math (the delta/resample half of the old `sparkline`)
// ---------------------------------------------------------------------------

/// Per-bucket deltas of a **cumulative** series (the web `diffs()`): fewer than
/// two points pass through unchanged; otherwise adjacent differences, clamped
/// at 0 so a counter reset never dips negative.
pub(crate) fn diffs(cumulative: &[i64]) -> Vec<i64> {
    if cumulative.len() < 2 {
        return cumulative.to_vec();
    }
    cumulative
        .windows(2)
        .map(|w| (w[1] - w[0]).max(0))
        .collect()
}

/// Resample `deltas` to exactly `cells` buckets (sum within each source range;
/// nearest-neighbor stretch when `deltas` is shorter than `cells`).
pub(crate) fn resample(deltas: &[i64], cells: usize) -> Vec<i64> {
    let n = deltas.len();
    if n == 0 {
        return vec![0; cells];
    }
    (0..cells)
        .map(|i| {
            let lo = i * n / cells;
            let hi = ((i + 1) * n / cells).max(lo + 1).min(n);
            deltas[lo..hi].iter().copied().sum()
        })
        .collect()
}

/// A chart-ready series for a cumulative bucket key: diff, resample to `cells`,
/// clamp negatives. Fills `App.{saved,bytes,event}_series`.
pub(crate) fn series(s: &Value, key: &str, cells: usize) -> Vec<u64> {
    resample(&diffs(&buckets(s, key)), cells)
        .into_iter()
        .map(|v| v.max(0) as u64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn money_scales_with_magnitude() {
        assert_eq!(money(5.0), "$5.00");
        assert_eq!(money(0.05), "$0.050");
        assert_eq!(money(0.001), "$0.0010");
        // -0.0 renders as a clean zero, matching the web's toFixed (no leading "-").
        assert_eq!(money(-0.0), "$0.0000");
    }

    #[test]
    fn model_label_maps_canonical_keys() {
        assert_eq!(model_label("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(model_label("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(model_label("claude-haiku-4-5"), "Haiku 4.5");
        assert_eq!(model_label("claude-fable-5"), "Fable 5");
        assert_eq!(model_label("claude-mythos-x"), "mythos-x");
        // A dated snapshot id drops both the prefix and the trailing date.
        assert_eq!(model_label("claude-haiku-4-5-20251001"), "haiku-4-5");
    }

    #[test]
    fn diffs_resample_and_series_preserve_bucket_math() {
        // The delta math the old `sparkline` fused with block rendering.
        assert_eq!(diffs(&[0, 1, 3, 6, 10]), vec![1, 2, 3, 4]);
        assert_eq!(diffs(&[5, 5, 5, 5]), vec![0, 0, 0]);
        // Fewer than two points pass through unchanged.
        assert_eq!(diffs(&[7]), vec![7]);
        assert!(diffs(&[]).is_empty());
        // A drop (counter reset) clamps at 0 rather than going negative.
        assert_eq!(diffs(&[10, 4]), vec![0]);

        // Resample sums within each range when shrinking, stretches when growing.
        assert_eq!(resample(&[1, 2, 3, 4], 2), vec![3, 7]);
        assert_eq!(resample(&[2], 4), vec![2, 2, 2, 2]);
        assert_eq!(resample(&[], 3), vec![0, 0, 0]);

        // series = resample(diffs(buckets)) clamped to u64, exactly `cells` long.
        let snap = serde_json::json!({ "saved_buckets": [0, 4, 4, 10] });
        assert_eq!(series(&snap, "saved_buckets", 3), vec![4, 0, 6]);
        assert_eq!(series(&snap, "missing", 5), vec![0; 5]);
    }
}
