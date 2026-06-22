//! Emits two files from one loaded set of results:
//!
//!   * `BENCHMARKS.md` — results-first headline doc (realistic-scale savings,
//!     accuracy, recovery, small-N caveats, a short honesty footer, appendix link).
//!   * `BENCHMARKS_APPENDIX.md` — the full audit trail (methodology, the
//!     1×/10×/50× scale curve + classification, the Context Mode `n/a` reasoning,
//!     the discovery-regression investigation, raw-bytes baselines).
//!
//!   cargo run --bin bench_report
//!
//! Presentation only. Savings/scale are recomputed live (deterministic, exactly
//! as before); accuracy/recovery are read from the committed
//! `{accuracy,recovery}/results/{real,mock}.json`. No measured value is changed —
//! the split between headline and appendix is driven entirely by this generator,
//! so a future re-run after new data regenerates both files with no hand-editing.

#[allow(dead_code)]
#[path = "../common/accuracy.rs"]
mod accuracy;
#[allow(dead_code)]
#[path = "../common/recovery.rs"]
mod recovery;
#[allow(dead_code)]
#[path = "../common/savings.rs"]
mod savings;

use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize)]
struct AccuracyReport {
    mode: String,
    model: String,
    groups: Vec<accuracy::Group>,
}

#[derive(Deserialize)]
struct RecoveryReport {
    mode: String,
    model: String,
    groups: Vec<recovery::Group>,
}

const METHODOLOGY: &str = r#"## Methodology

ctxforge is benchmarked against the metrics the **headroom** project publishes,
but matched to where ctxforge actually sits in the loop. There are two halves,
and they are not the same kind of measurement.

**Savings** is directly comparable to headroom's proof table: tokens entering
context **without** ctxforge (a realistic naive-agent path) vs **with** it.
Token estimate = bytes / 4 (the same rough convention `ctx_stats` uses); raw
byte counts are shown alongside. Every row is segmented by the ctxforge tool
that produced the saving (`sandbox` / `index` / `compression` / `discovery`),
because ctxforge saves via different mechanisms than headroom — it mostly
*prevents* data entering context, where headroom *compresses* data that does. A
single blended percentage would hide which mechanism did the work.

**Accuracy** uses a task-based method, **not** GSM8K/TruthfulQA. Those measure
whether compressing a *prompt* preserves answer accuracy — faithful for a
prompt-path compressor like headroom. ctxforge is an MCP tool provider that sits
*beside* the prompt path; nothing forces a QA prompt through `ctx_execute`. So
the faithful accuracy question is: *when the agent uses the sandbox / graph /
search instead of reading raw files, does it still answer correctly?* Each task
is run twice with the same model — **control** (raw fixtures, capped at a naive
context budget) vs **treatment** (the ctxforge tool's compact output) — and
scored against deterministic ground truth. The result we want to state honestly
is **Δ acc ≈ 0 with a large token reduction**. A negative Δ on any mechanism is
surfaced loudly: it means that mechanism is dropping load-bearing context.

Without `ANTHROPIC_API_KEY` the accuracy harness runs in **mock mode** (a
context-presence oracle that tests scoring/plumbing only) and the table below is
marked pending a real-model run.

"#;

const ISOLATION_NOTE: &str = r#"### Context Mode isolation + head-to-head

These savings come from `cargo run --bin bench_savings`, a standalone Rust binary
that calls ctxforge's library functions **directly** (index / sandbox /
compression / discovery) — it does not route through any MCP server or hook, so
Context Mode's PreToolUse hooks cannot intercept the workload. The numbers are
ctxforge's own.

**Context Mode (measured), same machine, same workloads.** CM is comparable only
where it has an equivalent mechanism:

| Workload | ctxforge mechanism | Context Mode (measured) |
| --- | --- | --- |
| Code search | FTS5 index → ranked snippets | `n/a` — CM `ctx_index`/`ctx_search` index into a session-global FTS5 KB; the per-workload token figure can't be isolated from session state without faking it. |
| Log debugging | sandboxed grep, matches only | `n/a` — CM `ctx_execute` runs the same grep; equivalent by construction, no independent CM compaction to measure. |
| Issue triage | columnar + dictionary JSON compaction | `n/a` — CM has no structural-JSON compactor; this is the headroom/SmartCrusher archetype, not a CM mechanism. |
| Codebase exploration | tree-sitter code graph | `n/a` — CM has no code graph. |

Every CM cell is `n/a` with a stated reason rather than a fabricated number. The
faithful head-to-head ctxforge *was* built to win is **session recovery** (below),
which drives CM's real hook scripts.

"#;

/// Intro paragraph for the clean doc: what ctxforge is + the appendix link. No
/// methodology essay (that lives in the appendix).
const INTRO: &str = r#"ctxforge is an MCP tool provider that keeps work **out** of the agent's context window: it indexes, sandboxes, compresses, and graphs data so the bytes a naive agent would read never enter context. The tables below are the measured results.

_Full scale curves, mechanism classifications, the discovery-regression investigation, and methodology are in [BENCHMARKS_APPENDIX.md](BENCHMARKS_APPENDIX.md)._

"#;

/// The recovery-section intro (the bar is Context Mode, not ctxforge's own sense
/// of working). Kept short in the clean doc.
const RECOVERY_INTRO: &str = "Proves the Context Mode replacement: each scenario builds a working state, forces a compaction boundary, then asks a question only answerable if the state survived. The bar is **Context Mode**, not ctxforge's own sense of working — the swap is only safe when **ctxforge ≥ Context Mode** at comparable token cost.\n\n";

/// Short two-sentence honesty footer for the clean doc (§2.5).
const HONESTY_FOOTER: &str = r#"- Context Mode has no JSON-compactor or code-graph equivalent, so three of the four savings workloads have no faithful Context Mode head-to-head (full per-cell reasoning in the appendix); the one faithful Context Mode comparison is **session recovery**, above.
- The real-model runs were obtained via `claude-pty` on plan quota; the supported path for reproduction is a direct `ANTHROPIC_API_KEY` run (see the appendix and [benchmarks/README.md](benchmarks/README.md)).
"#;

/// The discovery-regression investigation — the apparatus catching its own bad
/// number. Same trail recorded in DECISIONS.md, kept whole in the appendix.
const DISCOVERY_INVESTIGATION: &str = r#"## The discovery-regression investigation

This is the proof the apparatus catches its own bad numbers; it is kept whole.

The first real accuracy run, on `claude-haiku-4-5`, showed **discovery −33pp** (N = 3, so one task = 33pp). The generator's auto-warning fired — it flags *any* negative aggregate delta as "dropping load-bearing context". Per-task investigation showed the opposite. The one regressing task (`0008_reachable_path`, "can `handle_request` reach `connect_db`?") has a treatment context — the `graph_path` op — that returns the **correct** answer (`found:true`, full path `handle_request → fetch_user → connect_db`), *more* explicit than the raw-file control, yet Haiku still answered `reachable:false`. That is a weak-model reasoning slip on a correct context, **not** a ctxforge context-drop.

Re-running just the discovery set on `claude-sonnet-4-6` (same backend) confirmed it: discovery returns to **100% / 100% (+0pp)**, with `0008` answering `reachable:yes`. The slip disappears on the stronger model.

Lesson, encoded in `bench_report`: a negative aggregate delta is *necessary-not-sufficient* evidence of a context-drop. The ⚠️ on the aggregate is a heuristic; per-task plus cross-model checks separate a real regression from model noise before the table is trusted.

"#;

/// "6 / 3 / 2"-style sample-size string from a group's N column.
fn small_n(ns: &[usize]) -> String {
    ns.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(" / ")
}

/// Build the provenance + delta-status note for a real accuracy run. Generic
/// across models: states the claude-pty/plan-quota provenance and whether any
/// mechanism showed a negative delta this run (the detailed cross-model
/// investigation of the historical Haiku discovery slip lives in DECISIONS.md).
fn accuracy_real_note(groups: &[accuracy::Group], model_label: &str) -> String {
    let negatives: Vec<&str> = groups
        .iter()
        .filter(|g| g.treatment_acc < g.control_acc)
        .map(|g| g.mechanism.as_str())
        .collect();
    let mut s = String::from(
        "\n> **Real run via `claude-pty`** (interactive Claude Code, plan quota — no API credit), tools disabled so each arm answers only from its given context, same isolation as a direct API call.\n",
    );
    if negatives.is_empty() {
        s.push_str(&format!(
            ">\n> Every mechanism is **≥ control** on `{model_label}` — no negative accuracy delta this run. The token reductions are the savings; accuracy is preserved.\n"
        ));
    } else {
        s.push_str(&format!(
            ">\n> Negative delta this run on: **{}**. A negative aggregate delta can be either ctxforge scoping out load-bearing context *or* a weak-model slip on a context that was actually correct — per-task investigation distinguishes them (see the discovery-slip writeup in DECISIONS.md for a worked example). The ⚠️ above is a heuristic on the aggregate and fires before that distinction.\n",
            negatives.join(", ")
        ));
    }
    s
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // --- Savings (recomputed live, deterministic) ---------------------------
    let rows = savings::compute_savings().await?;
    let scale = savings::compute_scale_curve();
    let headline_md = savings::render_headline_savings_markdown(&rows, &scale); // clean doc
    let savings_full_md = savings::render_savings_markdown(&rows); // appendix
    let scale_md = savings::render_scale_curve_markdown(&scale); // appendix

    // --- Accuracy (read committed results; prefer real, else mock) ----------
    let results_dir = savings::bench_root().join("accuracy/results");
    let acc_path = first_existing(&[results_dir.join("real.json"), results_dir.join("mock.json")]);
    // (clean accuracy section, full appendix accuracy section)
    let (clean_accuracy_md, appendix_accuracy_md) = match acc_path {
        Some(p) => {
            let raw = std::fs::read_to_string(&p)?;
            let rep: AccuracyReport = serde_json::from_str(&raw)?;
            let is_mock = rep.mode == "mock";
            let table = accuracy::render_accuracy_markdown(&rep.groups, &rep.model, is_mock);

            // Clean: table + run-method line (real only) + REQUIRED small-N caveat.
            let ns: Vec<usize> = rep.groups.iter().map(|g| g.n).collect();
            let mut clean = table.clone();
            if rep.mode == "real" {
                clean.push_str("\n> Run method: real model via `claude-pty`, tools disabled, context-only isolation — each arm answers only from its given context, exactly like a direct API call.\n");
            }
            clean.push_str(&format!(
                ">\n> Samples are small (N = {}); these are directional confirmations consistent with the mechanism analysis, not statistically powered rates.\n",
                small_n(&ns)
            ));

            // Appendix: table + the full provenance / delta-status note.
            let mut appendix = table;
            if rep.mode == "real" {
                appendix.push_str(&accuracy_real_note(&rep.groups, &rep.model));
            }
            (clean, appendix)
        }
        None => {
            let pending =
                "_Accuracy harness has not been run. Run `cargo run --bin bench_accuracy`._\n"
                    .to_string();
            (pending.clone(), pending)
        }
    };

    // --- Recovery (the faithful Context Mode head-to-head; clean doc only) ---
    let rec_dir = savings::bench_root().join("recovery/results");
    let rec_path = first_existing(&[rec_dir.join("real.json"), rec_dir.join("mock.json")]);
    let recovery_md = match rec_path {
        Some(p) => {
            let raw = std::fs::read_to_string(&p)?;
            let rep: RecoveryReport = serde_json::from_str(&raw)?;
            let mut md =
                recovery::render_recovery_markdown(&rep.groups, &rep.model, rep.mode == "mock");
            let ns: Vec<usize> = rep.groups.iter().map(|g| g.n).collect();
            md.push_str(&format!(
                "\n_Samples are small (N = {}); directional confirmations, not statistically powered rates._\n",
                small_n(&ns)
            ));
            md
        }
        None => "_Recovery harness has not been run. Run `cargo run --bin bench_recovery`._\n"
            .to_string(),
    };

    // --- Clean, results-first BENCHMARKS.md ---------------------------------
    let clean = format!(
        "# ctxforge benchmarks\n\n{INTRO}## Savings\n\n{headline_md}\n## Accuracy\n\n{clean_accuracy_md}\n## Session recovery\n\n{RECOVERY_INTRO}{recovery_md}\n## Notes\n\n{HONESTY_FOOTER}"
    );

    // --- Full audit trail BENCHMARKS_APPENDIX.md ----------------------------
    let appendix = format!(
        "# ctxforge benchmarks — appendix\n\n_This is the full measurement trail behind [BENCHMARKS.md](BENCHMARKS.md). Nothing here is recomputed; it is the same committed data, shown in full._\n\n{METHODOLOGY}## Savings (full)\n\n{savings_full_md}\n{scale_md}\n{ISOLATION_NOTE}## Accuracy (full)\n\n{appendix_accuracy_md}\n{DISCOVERY_INVESTIGATION}"
    );

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let clean_out = manifest.join("BENCHMARKS.md");
    let appendix_out = manifest.join("BENCHMARKS_APPENDIX.md");
    std::fs::write(&clean_out, clean)?;
    std::fs::write(&appendix_out, appendix)?;
    eprintln!("wrote {}", clean_out.display());
    eprintln!("wrote {}", appendix_out.display());
    Ok(())
}

/// First path in `candidates` that exists on disk, if any.
fn first_existing(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates.iter().find(|p| p.exists()).cloned()
}
