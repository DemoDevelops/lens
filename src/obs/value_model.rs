//! The **applied-value model** — benchmark per-op rates applied to the user's live
//! op counts, so the dashboard accumulates real session value the byte-delta ledger
//! scores as zero.
//!
//! The `$`-saved headline already sums the *measured* byte-delta (`raw − returned`),
//! which captures darkroom and skeleton honestly. But `lens_search`, the graph nav
//! tools, and `lens_recall` return ~as many bytes as they ingest, so their value is
//! entirely *counterfactual*: the grep+read chains the agent never ran, the
//! re-derivation it never paid. The benchmark suite measured those counterfactuals on
//! real `claude-opus-4-8`; this module turns them into per-op rates and multiplies by
//! the user's actual op counts.
//!
//! Two kinds of value per op:
//!   * `est_tokens_per_op` — counterfactual tokens saved, for dimensions whose live
//!     byte-delta is ~zero (search, recall). It is 0 where the saving is already
//!     *measured* (darkroom, skeleton) or ~neutral (navigation), so nothing
//!     double-counts the headline.
//!   * `round_trips_per_op` — agent tool round-trips avoided (grep→read→read chains
//!     collapsed). This is the latency driver; the renderer prices it at a
//!     `--rt-seconds` constant (default [`ROUND_TRIP_SECONDS`]) for "time saved".
//!
//! Honesty: the HARD rates (navigation round-trips, recall tokens, search tokens) are
//! recomputed from the committed JSONs by the drift-guard below. The per-op
//! round-trip floor of 1 (a value op replaced at least one `Read`) and the seconds
//! constant are *modeling assumptions*, documented as such and surfaced, never buried.
//! Everything here is an estimate; it never enters the measured `$` ledger.

use serde_json::Value;

/// One per-tool applied-value rate, derived from the committed benchmarks.
pub struct ValueRate {
    /// The MCP tool whose live op count this rate multiplies.
    pub tool: &'static str,
    /// The value dimension: `darkroom | skeleton | search | navigation | recovery`.
    pub dimension: &'static str,
    /// Counterfactual tokens saved per call (0 where the saving is measured live).
    pub est_tokens_per_op: f64,
    /// Agent round-trips avoided per call (the latency driver).
    pub round_trips_per_op: f64,
    /// Human provenance, shown in the row tooltip.
    pub basis: &'static str,
    /// Committed JSON backing a HARD rate, or `""` for a modeling floor.
    pub source: &'static str,
}

/// Default seconds per avoided agent round-trip (a tool call plus the model turn that
/// reads its result). Conservative; override with `--rt-seconds`. The only modeled
/// time constant, and it is always shown beside the time figure.
pub const ROUND_TRIP_SECONDS: f64 = 4.0;

/// The model + harness the benchmark rates were measured on.
pub const VALUE_MODEL_MODEL: &str = "claude-opus-4-8 (via claude-pty)";

/// Caption both surfaces show, so these never read as live measurements.
pub const VALUE_MODEL_NOTE: &str =
    "benchmark rates applied to your live ops — estimated, not measured this session";

const NAVIGATION_SRC: &str = "benchmarks/navigation/expected/navigation.json";
const RECOVERY_SRC: &str = "benchmarks/recovery/results/real.json";
const SAVINGS_SRC: &str = "benchmarks/savings/expected/savings.json";

// HARD rates, each recomputed from its JSON by the drift-guard:
//   navigation: (Σ naive_round_trips − Σ graph_round_trips) / queries = (36 − 11)/11
const NAV_ROUND_TRIPS: f64 = (36.0 - 11.0) / 11.0;
//   recall: mean over recovery sets of (context-mode tokens − lens tokens)
const RECALL_TOKENS: f64 = ((4622.0 - 205.0) + (4677.0 - 291.0)) / 2.0;
//   search: index-workload token saving spread over its 6 bundled queries
const SEARCH_QUERIES: f64 = 6.0;
const SEARCH_TOKENS: f64 = (3681.0 - 2383.0) / SEARCH_QUERIES;
/// Conservative floor: any value-producing op replaced at least one `Read` round-trip.
const READ_FLOOR: f64 = 1.0;

/// The display order of value dimensions.
pub const VALUE_DIMENSIONS: &[&str] =
    &["darkroom", "skeleton", "search", "navigation", "recovery"];

/// Per-tool rates. darkroom/skeleton carry `est_tokens=0` (their tokens are measured
/// live, in the `$` headline) and a 1-round-trip floor. search/recall carry HARD
/// counterfactual token rates. navigation carries the HARD measured round-trip rate.
pub const VALUE_RATES: &[ValueRate] = &[
    ValueRate {
        tool: "lens_run",
        dimension: "darkroom",
        est_tokens_per_op: 0.0,
        round_trips_per_op: READ_FLOOR,
        basis: "file load avoided; tokens measured live (savings.json darkroom 93%)",
        source: "",
    },
    ValueRate {
        tool: "lens_run_file",
        dimension: "darkroom",
        est_tokens_per_op: 0.0,
        round_trips_per_op: READ_FLOOR,
        basis: "file load avoided; tokens measured live",
        source: "",
    },
    ValueRate {
        tool: "lens_skeleton",
        dimension: "skeleton",
        est_tokens_per_op: 0.0,
        round_trips_per_op: READ_FLOOR,
        basis: "full read avoided; tokens measured live (savings.json skeleton 64%)",
        source: "",
    },
    ValueRate {
        tool: "lens_search",
        dimension: "search",
        est_tokens_per_op: SEARCH_TOKENS,
        round_trips_per_op: READ_FLOOR,
        basis: "savings.json index: 1298 tok over 6 queries; files not read",
        source: SAVINGS_SRC,
    },
    ValueRate {
        tool: "lens_index",
        dimension: "search",
        est_tokens_per_op: 0.0,
        round_trips_per_op: 0.0,
        basis: "index build (setup; value lands on the searches it enables)",
        source: "",
    },
    ValueRate {
        tool: "lens_symbol",
        dimension: "navigation",
        est_tokens_per_op: 0.0,
        round_trips_per_op: NAV_ROUND_TRIPS,
        basis: "navigation.json: 36 → 11 round-trips over 11 define/callers/path queries",
        source: NAVIGATION_SRC,
    },
    ValueRate {
        tool: "lens_links",
        dimension: "navigation",
        est_tokens_per_op: 0.0,
        round_trips_per_op: NAV_ROUND_TRIPS,
        basis: "navigation.json: grep+read chain collapsed to one query",
        source: NAVIGATION_SRC,
    },
    ValueRate {
        tool: "lens_path",
        dimension: "navigation",
        est_tokens_per_op: 0.0,
        round_trips_per_op: NAV_ROUND_TRIPS,
        basis: "navigation.json: source-subtree trace collapsed to one query",
        source: NAVIGATION_SRC,
    },
    ValueRate {
        tool: "lens_find",
        dimension: "navigation",
        est_tokens_per_op: 0.0,
        round_trips_per_op: NAV_ROUND_TRIPS,
        basis: "navigation.json: symbol lookup collapsed to one query",
        source: NAVIGATION_SRC,
    },
    ValueRate {
        tool: "lens_recall",
        dimension: "recovery",
        est_tokens_per_op: RECALL_TOKENS,
        round_trips_per_op: READ_FLOOR,
        basis: "recovery.json: ~4.4K tok to recall vs re-derive from context-mode",
        source: RECOVERY_SRC,
    },
];

/// Sum the applied value over `VALUE_RATES`, weighted by `ops_for(tool)`, into the
/// snapshot block. `measured_tokens` is the session's measured savings
/// (`tokens_saved_mcp`); the block reports it alongside the estimated counterfactual so
/// both renderers can show measured vs estimated. Time is left to the renderer
/// (`round_trips_avoided × rt_seconds`), mirroring how `$` prices measured tokens.
pub fn applied_value_json(ops_for: impl Fn(&str) -> u64, measured_tokens: i64) -> Value {
    // dimension -> (ops, est_tokens, round_trips), in VALUE_DIMENSIONS order.
    let mut acc: Vec<(u64, f64, f64)> = vec![(0, 0.0, 0.0); VALUE_DIMENSIONS.len()];
    let (mut est_total, mut rt_total) = (0.0_f64, 0.0_f64);
    for r in VALUE_RATES {
        let n = ops_for(r.tool);
        if n == 0 {
            continue;
        }
        let idx = VALUE_DIMENSIONS.iter().position(|d| *d == r.dimension).unwrap();
        let (ops, est, rt) = &mut acc[idx];
        *ops += n;
        *est += n as f64 * r.est_tokens_per_op;
        *rt += n as f64 * r.round_trips_per_op;
        est_total += n as f64 * r.est_tokens_per_op;
        rt_total += n as f64 * r.round_trips_per_op;
    }
    let rows: Vec<Value> = VALUE_DIMENSIONS
        .iter()
        .enumerate()
        .map(|(i, dim)| {
            let (ops, est, rt) = acc[i];
            serde_json::json!({
                "dimension": dim,
                "ops": ops,
                "est_tokens": est.round() as i64,
                "round_trips": (rt * 100.0).round() / 100.0,
                "source": dimension_source(dim),
            })
        })
        .collect();
    serde_json::json!({
        "model": VALUE_MODEL_MODEL,
        "note": VALUE_MODEL_NOTE,
        "estimate": true,
        "measured_tokens": measured_tokens,
        "est_counterfactual_tokens": est_total.round() as i64,
        "est_total_tokens": measured_tokens + est_total.round() as i64,
        "round_trips_avoided": (rt_total * 100.0).round() / 100.0,
        "rows": rows,
    })
}

/// The committed JSON a dimension's HARD rate traces to (empty for measured dims).
fn dimension_source(dim: &str) -> &'static str {
    VALUE_RATES
        .iter()
        .find(|r| r.dimension == dim && !r.source.is_empty())
        .map(|r| r.source)
        .unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn load(rel: &str) -> Value {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        let s = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()));
        serde_json::from_str(&s).unwrap_or_else(|e| panic!("parse {rel}: {e}"))
    }
    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }
    fn rate(tool: &str) -> &'static ValueRate {
        VALUE_RATES.iter().find(|r| r.tool == tool).unwrap()
    }

    /// DRIFT GUARD: every HARD per-op rate must recompute from its committed JSON.
    /// This is the load-bearing honesty test — if a benchmark moves, the rate fails
    /// here until it is re-derived.
    #[test]
    fn rates_trace_to_committed_benchmarks() {
        // Navigation round-trips = (Σ naive − Σ graph) / queries.
        let nav = load(NAVIGATION_SRC);
        let arr = nav.as_array().unwrap();
        let naive: i64 = arr.iter().map(|q| q["naive_round_trips"].as_i64().unwrap()).sum();
        let graph: i64 = arr.iter().map(|q| q["graph_round_trips"].as_i64().unwrap()).sum();
        let nav_rt = (naive - graph) as f64 / arr.len() as f64;
        for tool in ["lens_symbol", "lens_links", "lens_path", "lens_find"] {
            assert!(
                approx(rate(tool).round_trips_per_op, nav_rt),
                "{tool} nav rt {} != computed {nav_rt}",
                rate(tool).round_trips_per_op
            );
        }

        // Recall tokens = mean over recovery sets of (context-mode − lens).
        let rec = load(RECOVERY_SRC);
        let groups = rec["groups"].as_array().unwrap();
        let recall: f64 = groups
            .iter()
            .map(|g| g["cm_tokens"].as_f64().unwrap() - g["lens_tokens"].as_f64().unwrap())
            .sum::<f64>()
            / groups.len() as f64;
        assert!(
            approx(rate("lens_recall").est_tokens_per_op, recall),
            "recall tok {} != computed {recall}",
            rate("lens_recall").est_tokens_per_op
        );
        assert_eq!(rec["model"], json!(VALUE_MODEL_MODEL), "recovery model label");

        // Search tokens = index-workload saving / its 6 bundled queries.
        let sav = load(SAVINGS_SRC);
        let idx = sav
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["mechanism"] == json!("index"))
            .unwrap();
        let search = (idx["before_tokens"].as_f64().unwrap() - idx["after_tokens"].as_f64().unwrap())
            / SEARCH_QUERIES;
        assert!(
            approx(rate("lens_search").est_tokens_per_op, search),
            "search tok {} != computed {search}",
            rate("lens_search").est_tokens_per_op
        );
        // The 6-query divisor is a real property of that workload.
        assert!(
            idx["detail"].as_str().unwrap().contains("6 queries"),
            "savings index workload must document its query count"
        );
    }

    /// Modeling constants are the documented values (not silently drifting): measured
    /// dims carry 0 estimated tokens, value ops carry the 1-round-trip floor.
    #[test]
    fn modeling_assumptions_are_explicit() {
        for tool in ["lens_run", "lens_run_file", "lens_skeleton"] {
            assert_eq!(rate(tool).est_tokens_per_op, 0.0, "{tool} tokens are measured, not estimated");
            assert_eq!(rate(tool).round_trips_per_op, READ_FLOOR);
        }
        for tool in ["lens_symbol", "lens_links", "lens_path", "lens_find"] {
            assert_eq!(rate(tool).est_tokens_per_op, 0.0, "navigation value is round-trips, not tokens");
        }
        assert_eq!(ROUND_TRIP_SECONDS, 4.0);
        // Setup tools claim nothing.
        assert_eq!(rate("lens_index").round_trips_per_op, 0.0);
    }

    /// `applied_value_json` rolls ops into per-dimension rows and the right totals.
    #[test]
    fn applied_value_rolls_up_live_ops() {
        // 2 recalls, 3 nav (symbol), 1 search, no measured ops.
        let counts = |t: &str| -> u64 {
            match t {
                "lens_recall" => 2,
                "lens_symbol" => 3,
                "lens_search" => 1,
                _ => 0,
            }
        };
        let v = applied_value_json(counts, 1000);
        assert_eq!(v["estimate"], json!(true));
        assert_eq!(v["measured_tokens"], json!(1000));

        // est counterfactual = 2*RECALL + 1*SEARCH.
        let expect_est = (2.0 * RECALL_TOKENS + SEARCH_TOKENS).round() as i64;
        assert_eq!(v["est_counterfactual_tokens"], json!(expect_est));
        assert_eq!(v["est_total_tokens"], json!(1000 + expect_est));

        // round-trips = 2*recall floor + 3*nav rate + 1*search floor.
        let expect_rt = ((2.0 * READ_FLOOR + 3.0 * NAV_ROUND_TRIPS + READ_FLOOR) * 100.0).round() / 100.0;
        assert_eq!(v["round_trips_avoided"], json!(expect_rt));

        // Rows present for all 5 dimensions, in order.
        let rows = v["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 5);
        let dims: Vec<&str> = rows.iter().map(|r| r["dimension"].as_str().unwrap()).collect();
        assert_eq!(dims, VALUE_DIMENSIONS);
        let nav = rows.iter().find(|r| r["dimension"] == json!("navigation")).unwrap();
        assert_eq!(nav["ops"], json!(3));
        let rec = rows.iter().find(|r| r["dimension"] == json!("recovery")).unwrap();
        assert_eq!(rec["est_tokens"], json!((2.0 * RECALL_TOKENS).round() as i64));
    }

    /// With zero live ops the block still has all five rows (stable panel).
    #[test]
    fn zero_ops_still_lists_dimensions() {
        let v = applied_value_json(|_| 0, 0);
        assert_eq!(v["rows"].as_array().unwrap().len(), 5);
        assert_eq!(v["round_trips_avoided"], json!(0.0));
        assert_eq!(v["est_total_tokens"], json!(0));
    }
}
