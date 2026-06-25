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
    aggregate, default_model, load_tasks, render_accuracy_markdown, run_task, Model, TaskResult,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Backend precedence: explicit `LENS_BENCH_BACKEND=claude-pty` (bills to
    // plan quota via interactive Claude Code) > Anthropic API key > mock.
    let backend = std::env::var("LENS_BENCH_BACKEND").unwrap_or_default();
    let has_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let (model, pending, mode) = if backend == "claude-pty" {
        eprintln!("running accuracy harness via claude-pty (plan quota, tools disabled)");
        (Model::ClaudePty(default_model()), false, "real")
    } else if has_key {
        (Model::Anthropic(default_model()), false, "real")
    } else {
        eprintln!(
            "ANTHROPIC_API_KEY not set — running accuracy harness in MOCK mode \
             (scoring/plumbing only, no real-model accuracy). Set the key for a real run."
        );
        (Model::Mock, true, "mock")
    };

    let mut tasks = load_tasks()?;
    // Optional focus filter: `LENS_BENCH_ONLY=<substr>` keeps only tasks
    // whose mechanism or id contains the substring (e.g. "discovery"). Used to
    // re-run a single mechanism without spending calls on the rest.
    let mut filtered = false;
    if let Ok(only) = std::env::var("LENS_BENCH_ONLY") {
        if !only.is_empty() {
            tasks.retain(|t| t.primary_mechanism.contains(&only) || t.id.contains(&only));
            filtered = true;
            eprintln!("filter LENS_BENCH_ONLY={only} -> {} task(s)", tasks.len());
        }
    }
    let mut results: Vec<TaskResult> = Vec::new();
    for task in &tasks {
        match run_task(task, &model).await {
            Ok(r) => results.push(r),
            Err(e) => eprintln!("task {} failed: {e}", task.id),
        }
    }

    let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/accuracy/results");
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
    print!(
        "{}",
        render_accuracy_markdown(&groups, &model.label(), pending)
    );

    let payload = serde_json::json!({
        "mode": mode,
        "model": model.label(),
        "groups": groups,
        "tasks": results,
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&payload)? + "\n")?;
    eprintln!("\nwrote {}", out_path.display());

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
            results.push(run_task(task, &Model::Mock).await.expect("run task"));
        }

        // Treatment surfaces the evidence for every task -> all treatment correct.
        assert!(
            results.iter().all(|r| r.treatment.correct),
            "every treatment arm should be correct under the mock oracle"
        );
        // The harness must be able to detect a wrong answer (control loses data).
        assert!(
            results.iter().any(|r| !r.control.correct),
            "at least one control arm should be wrong (truncation drops evidence)"
        );

        let groups = aggregate(&results);
        assert_eq!(groups.len(), 3, "expected darkroom/discovery/search groups");
        for g in &groups {
            assert!(
                g.treatment_acc >= g.control_acc,
                "{}: treatment acc {} < control acc {}",
                g.mechanism,
                g.treatment_acc,
                g.control_acc
            );
        }

        // Treatment must consume fewer tokens than control overall.
        let ctrl_tok: usize = results.iter().map(|r| r.control.tokens).sum();
        let treat_tok: usize = results.iter().map(|r| r.treatment.tokens).sum();
        assert!(
            treat_tok < ctrl_tok,
            "treatment tokens {treat_tok} should be < control tokens {ctrl_tok}"
        );
    }
}
