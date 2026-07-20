//! read_overview: Nth hand-Read before any map → lens_overview nudge. Filled by T6.

/// Default Nth-read threshold: after this many code-file Reads in a session with
/// no intervening `lens_overview` call, [`nudge`] is due. T8 supplies
/// the live count from a throttle counter reset by any `lens_overview`
/// call (this rail's prefix is `read_overview` → `rovr`, per the contract in
/// [`super`]).
const OVERVIEW_THRESHOLD_DEFAULT: u64 = 5;

/// Is the Nth-hand-Read-before-any-map nudge due? True once `reads_before_map`
/// has reached `threshold`. Once-per-session firing is a throttle concern for
/// T8, not this predicate.
pub fn overview_due(reads_before_map: u64, threshold: u64) -> bool {
    reads_before_map >= threshold
}

/// The Nth-read threshold, overridable via `LENS_READ_OVERVIEW_THRESHOLD` so an
/// A/B can move it without a recompile. Falls back to
/// [`OVERVIEW_THRESHOLD_DEFAULT`] when unset or unparseable.
pub fn threshold() -> u64 {
    std::env::var("LENS_READ_OVERVIEW_THRESHOLD")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(OVERVIEW_THRESHOLD_DEFAULT)
}

/// Deny reason for the rail's steering arm — the `lens_overview` guidance,
/// plus the one-shot promise: the deny resets the read counters, so the
/// verbatim retry always passes.
pub fn deny_reason(reads_before_map: u64) -> String {
    format!(
        "You've read {reads_before_map} files this session with no repo map — get the map in one call instead of reading file after file: lens_overview() returns a token-budgeted, PageRank-ranked map of the whole codebase. If the lens tools aren't loaded yet, load them first: ToolSearch(query: \"select:lens_overview,lens_symbol\"). This fires once per session — the same Read will pass if you re-run it verbatim."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_due_at_threshold() {
        assert!(overview_due(5, 5));
    }

    #[test]
    fn overview_due_below_threshold() {
        assert!(!overview_due(4, 5));
    }

    #[test]
    fn overview_due_above_threshold() {
        assert!(overview_due(6, 5));
    }

    #[test]
    fn deny_reason_names_the_call_count_and_retry_promise() {
        let r = deny_reason(5);
        assert!(r.contains("lens_overview"));
        assert!(r.contains('5'));
        assert!(r.contains("will pass if you re-run it verbatim"));
    }
}
