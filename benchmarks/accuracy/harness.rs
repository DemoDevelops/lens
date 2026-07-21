//! Accuracy benchmark harness.
//!
//! Runs every task in `tasks/*.json` through two arms (control = raw fixtures
//! capped at a naive budget; treatment = lens tool output) with the same
//! model, scores against deterministic ground truth, and emits the accuracy
//! table segmented by mechanism.
//!
//!   cargo run --bin bench_accuracy        # real model if ANTHROPIC_API_KEY set, else mock
//!
//! With no API key it runs in **mock mode** (a context-presence oracle that
//! tests scoring/plumbing) and clearly marks the result as pending a real run.

#[path = "../common/accuracy.rs"]
mod accuracy;

use std::path::PathBuf;

use accuracy::{
    adoption_report, aggregate, default_model, filter_tasks, function_report, lens_release_bin,
    load_task_set, load_tasks, render_accuracy_markdown, render_adoption_markdown,
    run_agentic_suite, run_task, set_label, Model, TaskResult,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `--probe[-all]`: deterministic feature off/on A/B (no LLM, no quota). For
    // each L51 (`graph_fused`) / L40 (`overview`) task it prints whether the
    // answer is surfaced with the feature OFF vs ON, the win/tie/regression
    // verdict, and the validity fields proving the task can actually exercise
    // the mechanism (so a tie is never silently confused with an un-testable
    // fixture). `--probe <substr>` limits to matching task ids.
    let cli: Vec<String> = std::env::args().collect();
    if let Some(pos) = cli.iter().position(|a| a == "--probe" || a == "--probe-all") {
        let only = if cli[pos] == "--probe" {
            cli.get(pos + 1).map(|s| s.as_str())
        } else {
            None
        };
        return probe_all(only).await;
    }

    // `--runs <n>`: repeat each arm n times and fold into mean±stddev (default 1,
    // reproducing the original single-shot behavior byte-for-byte).
    let runs: usize = cli
        .iter()
        .position(|a| a == "--runs")
        .and_then(|i| cli.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);

    // Backend precedence: explicit `LENS_BENCH_BACKEND=claude-headless|claude-pty|agentic|
    // opencode-agentic` > Anthropic API key > mock. `opencode-agentic` (aliases
    // `grok-agentic`, `opencode`) bills the configured provider (default
    // `xai/grok-4.5`) instead of Claude Code plan quota.
    let backend = std::env::var("LENS_BENCH_BACKEND").unwrap_or_default();
    let has_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let (model, pending, mode) = if backend == "claude-headless" || backend == "headless" {
        eprintln!("running accuracy harness via headless claude -p (plan quota, tools disabled)");
        (Model::ClaudeHeadless(default_model()), false, "real")
    } else if backend == "claude-pty" || backend == "pty" {
        eprintln!("running accuracy harness via claude-pty (plan quota, tools disabled)");
        (Model::ClaudePty(default_model()), false, "real")
    } else if backend == "agentic" {
        eprintln!("running accuracy harness via agentic claude -p (plan quota, tools live incl mcp__lens)");
        (Model::ClaudeAgentic(default_model()), false, "real")
    } else if backend == "opencode-agentic" || backend == "grok-agentic" || backend == "opencode" {
        let m = accuracy::default_opencode_model();
        eprintln!(
            "running accuracy harness via agentic opencode run (model={m}, tools live incl lens MCP)"
        );
        (Model::OpenCodeAgentic(m), false, "real")
    } else if has_key {
        (Model::Anthropic(default_model()), false, "real")
    } else {
        eprintln!(
            "ANTHROPIC_API_KEY not set — running accuracy harness in MOCK mode \
             (scoring/plumbing only, no real-model accuracy). Set the key for a real run, \
             or LENS_BENCH_BACKEND=opencode-agentic for Grok via opencode."
        );
        (Model::Mock, true, "mock")
    };

    let all_tasks = load_tasks()?; // unfiltered, for the lens_fn tag lookup below
    let tasks = load_tasks()?;

    // Two composable filters, applied as an intersection (see `filter_tasks`).
    // `LENS_BENCH_SET=<path>` pins the run to a frozen per-release task set (a
    // JSON array of ids), so a version's numbers are always over the SAME tasks
    // — the fix for a prior run that invalidly compared different-sized sets
    // across versions. `LENS_BENCH_ONLY=<substr>` narrows by id or mechanism.
    let set_env = std::env::var("LENS_BENCH_SET").ok().filter(|s| !s.is_empty());
    let set_ids: Option<Vec<String>> = match &set_env {
        Some(raw) => Some(load_task_set(raw)?),
        None => None,
    };
    let task_set = set_env.as_deref().map(set_label);
    let only = std::env::var("LENS_BENCH_ONLY").ok().filter(|s| !s.is_empty());
    let tasks = filter_tasks(tasks, set_ids.as_deref(), only.as_deref());
    if let Some(label) = &task_set {
        eprintln!("LENS_BENCH_SET={label} -> {} task(s)", tasks.len());
    }
    if let Some(only) = &only {
        eprintln!("LENS_BENCH_ONLY={only} -> {} task(s)", tasks.len());
    }
    // Only `LENS_BENCH_ONLY` triggers the merge-into-existing path below (a
    // partial re-run that updates its tasks in place); a `LENS_BENCH_SET` run is
    // the canonical full record for its version and writes fresh.
    let filtered = only.is_some();

    let mut results: Vec<TaskResult> = match &model {
        // Agentic backends canary-gate both arm configs once before scoring
        // (abort loudly on a broken arm) and record adoption misses instead of
        // dropping zero-lens cells.
        Model::ClaudeAgentic(id) => run_agentic_suite(&tasks, id, runs).await?,
        Model::OpenCodeAgentic(id) => {
            accuracy::run_opencode_agentic_suite(&tasks, id, runs).await?
        }
        _ => {
            let mut r = Vec::new();
            for task in &tasks {
                match run_task(task, &model, runs).await {
                    Ok(x) => r.push(x),
                    Err(e) => eprintln!("task {} failed: {e}", task.id),
                }
            }
            r
        }
    };

    // `LENS_BENCH_OUT` redirects results so concurrent runs don't clobber the
    // shared committed path (the default), enabling parallel trials.
    let out_dir = std::env::var_os("LENS_BENCH_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/accuracy/results"));
    std::fs::create_dir_all(&out_dir)?;
    let out_path = out_dir.join(format!("{mode}.json"));

    // Merge mode: a filtered re-run updates only its tasks in an existing
    // same-model real.json (replace by id, keep the rest), rather than
    // clobbering the full table. Lets a flaky subset be re-run in isolation.
    if filtered && out_path.exists() {
        if let Ok(raw) = std::fs::read_to_string(&out_path) {
            if let Ok(prev) = serde_json::from_str::<serde_json::Value>(&raw) {
                let prev_model = prev.get("model").and_then(|m| m.as_str()).unwrap_or("");
                if prev_model == model.label() {
                    let mut merged: Vec<TaskResult> = prev
                        .get("tasks")
                        .and_then(|t| serde_json::from_value(t.clone()).ok())
                        .unwrap_or_default();
                    let rerun: std::collections::HashSet<String> =
                        results.iter().map(|r| r.id.clone()).collect();
                    merged.retain(|t| !rerun.contains(&t.id));
                    merged.extend(results.iter().cloned());
                    merged.sort_by(|a, b| a.id.cmp(&b.id));
                    results = merged;
                    eprintln!(
                        "merged into existing {} ({} tasks total)",
                        model.label(),
                        results.len()
                    );
                } else {
                    eprintln!(
                        "WARNING: existing real.json model `{prev_model}` != `{}`; not merging, writing fresh subset",
                        model.label()
                    );
                }
            }
        }
    }

    let groups = aggregate(&results);
    println!("# lens accuracy benchmark\n");
    if let Some(label) = &task_set {
        println!("Task set: `{label}`\n");
    }
    print!(
        "{}",
        render_accuracy_markdown(&groups, &model.label(), pending)
    );
    // Per-model lens adoption: agentic-only (tools-off treatment arms call
    // nothing by construction), computed from the canary-scored zero-lens misses.
    let adoption = (backend == "agentic").then(|| adoption_report(&results));
    if let Some(a) = &adoption {
        print!("{}", render_adoption_markdown(a, &model.label()));
    }

    // The Contracts bench schema: `overall` + `per_function` folded across
    // whatever tasks carry a `lens_fn` tag (populated for every real_agentic_*
    // task). `bench_binary` names the single binary this run's hooks and
    // `--mcp-config` both resolved to, so a future hooks/mcp mismatch shows up
    // in the results header instead of silently voiding the run.
    let (overall, per_function) = function_report(&all_tasks, &results);
    let bench_binary =
        (backend == "agentic").then(|| lens_release_bin().to_string_lossy().to_string());

    let payload = serde_json::json!({
        "mode": mode,
        "model": model.label(),
        "task_set": task_set,
        "bench_binary": bench_binary,
        "adoption": adoption,
        "groups": groups,
        "tasks": results,
        "overall": overall,
        "per_function": per_function,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&payload)? + "\n")?;
    eprintln!("\nwrote {}", out_path.display());

    Ok(())
}

/// Deterministic L40 overview-focus off/on A/B over the real-fixture tasks (no
/// LLM). The answer is "surfaced" iff every `evidence` token is present in the
/// treatment context (the mock-oracle rule), so this measures focus's own recall
/// with zero model noise. Each overview task is classified from two exact facts:
/// whether the unfocused overview already contained the answer (off=HIT, a
/// regression guard) and whether the answer is in the full unbudgeted overview at
/// all (can_participate). A WIN is off=MISS -> on=HIT; a REGRESSION is the reverse.
async fn probe_all(only: Option<&str>) -> anyhow::Result<()> {
    use lens::discovery::{self, query as gquery};
    use std::collections::HashMap;

    let tasks = accuracy::load_tasks()?;
    let (mut n_win, mut n_reg, mut n_guard, mut n_nohelp, mut n_invalid) = (0, 0, 0, 0, 0);

    println!("# L40 overview-focus off/on probe (deterministic, no LLM)\n");
    for task in &tasks {
        if task.treatment.graph_op.as_deref() != Some("overview") {
            continue;
        }
        if let Some(f) = only {
            if !task.id.contains(f) {
                continue;
            }
        }

        std::env::set_var("LENS_OVERVIEW_FOCUS", "0");
        let off_ctx = accuracy::build_treatment_context(task).await?;
        std::env::set_var("LENS_OVERVIEW_FOCUS", "1");
        let on_ctx = accuracy::build_treatment_context(task).await?;
        std::env::remove_var("LENS_OVERVIEW_FOCUS");

        let present = |ctx: &str| task.evidence.iter().all(|e| ctx.contains(e));
        let off = present(&off_ctx);
        let on = present(&on_ctx);

        // Can the answer participate at all? It must be in the full unbudgeted
        // overview, so focus has something to lift into the tight budget.
        let fixture = accuracy::accuracy_root().join(&task.fixtures[0]);
        let graph = discovery::discover(&fixture, None)?.graph;
        let answer = task.evidence.first().cloned().unwrap_or_default();
        let can_participate = gquery::overview(&graph, 1_000_000, &HashMap::new()).contains(&answer);

        // off=HIT tasks are regression guards (the unfocused overview already had
        // the answer; the only question is whether focus knocks it out). off=MISS
        // tasks are improvement probes: a win lifts a below-budget answer into
        // budget; no-help means focus applied but did not lift it; INVALID means
        // the answer is not in the overview at all, so the task cannot test focus.
        let class = match (off, on, can_participate) {
            (false, true, _) => "WIN",
            (true, false, _) => "REGRESSION",
            (true, true, _) => "guard-held",
            (false, false, true) => "no-help",
            (false, false, false) => "INVALID",
        };
        match class {
            "WIN" => n_win += 1,
            "REGRESSION" => n_reg += 1,
            "guard-held" => n_guard += 1,
            "no-help" => n_nohelp += 1,
            _ => n_invalid += 1,
        }
        println!(
            "{:<32} off={:<4} on={:<4} participate={:<5} {class}",
            task.id,
            if off { "HIT" } else { "MISS" },
            if on { "HIT" } else { "MISS" },
            can_participate,
        );
    }
    println!("\nwin={n_win} regression={n_reg} guard-held={n_guard} no-help={n_nohelp} invalid={n_invalid}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::accuracy::*;
    use serde_json::json;

    #[test]
    fn scoring_exact_match() {
        let gt = json!({"distinct_error_types": 7, "most_frequent": "ConnectionTimeout"});
        assert!(score(
            &json!({"distinct_error_types": 7, "most_frequent": "connectiontimeout"}),
            &gt,
            "exact_match",
            None
        ));
        assert!(!score(
            &json!({"distinct_error_types": 6, "most_frequent": "ConnectionTimeout"}),
            &gt,
            "exact_match",
            None
        ));
        // string number coerces
        assert!(score(
            &json!({"distinct_error_types": "7", "most_frequent": "ConnectionTimeout"}),
            &gt,
            "exact_match",
            None
        ));
    }

    #[test]
    fn scoring_contains_and_numeric() {
        assert!(score(
            &json!({"file": "src/db.rs"}),
            &json!({"file": "db.rs"}),
            "contains",
            None
        ));
        assert!(!score(
            &json!({"file": "auth.rs"}),
            &json!({"file": "db.rs"}),
            "contains",
            None
        ));
        assert!(score(
            &json!({"port": 8080}),
            &json!({"port": 8080}),
            "numeric_tolerance",
            Some(0.0)
        ));
        assert!(score(
            &json!({"port": "8080"}),
            &json!({"port": 8080}),
            "numeric_tolerance",
            Some(0.0)
        ));
        assert!(!score(
            &json!({"port": 9090}),
            &json!({"port": 8080}),
            "numeric_tolerance",
            Some(0.0)
        ));
        // missing key fails
        assert!(!score(
            &json!({}),
            &json!({"port": 8080}),
            "numeric_tolerance",
            None
        ));
    }

    #[test]
    fn scoring_yes_no_bool_equivalence() {
        // A yes/no prompt answered as a boolean is correct: `lens_path` returns
        // `found:true`, which primes the model to answer `{"reachable": true}`
        // instead of the string "yes" (0008_reachable_path).
        let gt = json!({"reachable": "yes"});
        assert!(score(&json!({"reachable": true}), &gt, "contains", None));
        assert!(score(&json!({"reachable": "yes"}), &gt, "contains", None));
        assert!(score(&json!({"reachable": true}), &gt, "exact_match", None));
        // a wrong predicate is still wrong, either form
        assert!(!score(&json!({"reachable": false}), &gt, "contains", None));
        assert!(!score(&json!({"reachable": "no"}), &gt, "exact_match", None));
        // non-predicate strings are unaffected: a bool answer to a file
        // question stays wrong, and file-name contains still works.
        assert!(!score(&json!({"file": true}), &json!({"file": "db.rs"}), "contains", None));
        assert!(score(
            &json!({"file": "src/db.rs"}),
            &json!({"file": "db.rs"}),
            "contains",
            None
        ));
    }

    #[test]
    fn mock_oracle_presence() {
        let gt = json!({"count": 12});
        // evidence present -> ground truth
        assert_eq!(
            mock_answer(
                "...ConnectionTimeout...",
                &["ConnectionTimeout".into()],
                &gt
            ),
            gt
        );
        // evidence absent -> UNKNOWN
        assert_eq!(
            mock_answer("nothing here", &["ConnectionTimeout".into()], &gt),
            json!({"count": "UNKNOWN"})
        );
    }

    // End-to-end mock run: exercises context building, tool execution, scoring,
    // and aggregation without spending API calls.
    #[tokio::test]
    async fn mock_run_end_to_end() {
        let tasks = load_tasks().expect("load tasks");
        assert!(
            tasks.len() >= 10,
            "expected >= 10 tasks, got {}",
            tasks.len()
        );

        let mut results = Vec::new();
        for task in &tasks {
            results.push(run_task(task, &Model::Mock, 1).await.expect("run task"));
        }

        // Treatment surfaces the evidence for every savings task -> all correct.
        // Excludes the L36 `findloc` ranking-A/B fixtures: those probe the
        // boundary where personalized find DOES NOT surface the answer (the
        // knife-edge and tight-budget regression cases), so a missing-evidence
        // treatment there is the experiment, not a failure. They are measured by
        // their dedicated LENS_FIND_RANK runs, not this savings health-check.
        // The `real_focus_*` probes (real src/ subsystems) are the same:
        // reality-check A/B fixtures for L40 overview focus, measured by their
        // own LENS_OVERVIEW_FOCUS off/on run (`bench_accuracy --probe`).
        assert!(
            results
                .iter()
                .filter(|r| !r.id.contains("findloc") && !r.id.contains("real"))
                .all(|r| r.treatment.correct),
            "every savings treatment arm should be correct under the mock oracle"
        );
        // The harness must be able to detect a wrong answer (control loses data).
        assert!(
            results.iter().any(|r| !r.control.correct),
            "at least one control arm should be wrong (truncation drops evidence)"
        );

        let groups = aggregate(&results);
        assert_eq!(
            groups.len(),
            4,
            "expected darkroom/discovery/search/skeleton groups"
        );
        for g in &groups {
            assert!(
                g.treatment_acc >= g.control_acc,
                "{}: treatment acc {} < control acc {}",
                g.mechanism,
                g.treatment_acc,
                g.control_acc
            );
        }

        // Treatment must consume fewer tokens than control overall. Excludes the
        // L36 `findloc` ranking fixtures: those measure localization CORRECTNESS
        // at a fixed find budget, not byte savings, and a find subgraph that
        // surfaces the reachable hub is legitimately larger than a 2KB-truncated
        // raw-source control. They remain in every accuracy/correctness assertion
        // above. The `real_*` probes are likewise excluded: they point at real
        // src/ subsystems, so the 2KB-truncated control is not a meaningful
        // byte-savings baseline for them.
        let savings: Vec<&TaskResult> =
            results.iter().filter(|r| !r.id.contains("findloc") && !r.id.contains("real")).collect();
        let ctrl_tok: usize = savings.iter().map(|r| r.control.tokens).sum();
        let treat_tok: usize = savings.iter().map(|r| r.treatment.tokens).sum();
        assert!(
            treat_tok < ctrl_tok,
            "treatment tokens {treat_tok} should be < control tokens {ctrl_tok}"
        );
    }
}
