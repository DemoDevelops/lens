//! Shared accuracy-benchmark logic, `#[path]`-included by `harness.rs` and
//! `generate_report.rs`.
//!
//! Task-based, two-arm design (see benchmarks/README.md for why this, not
//! GSM8K). For each task we build two contexts for the *same* model: a `control`
//! context (the raw fixture bytes, capped at a naive-agent budget — the regime
//! where a real session truncates and misses things) and a `treatment` context
//! (the compact output of the lens tool the task names: darkroom stdout,
//! search snippets, or a graph view).
//!
//! The model answers from each; we score against deterministic ground truth and
//! record tokens consumed. The claim "same answers, fewer tokens" holds iff
//! treatment accuracy >= control accuracy while treatment tokens << control.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use lens::darkroom;
use lens::discovery::graph::Graph;
use lens::discovery::{self, query as gquery};
use lens::index::Index;
use lens::store::Store;
use lens::tools::{ExecuteRequest, GraphView, NodeView};

/// Naive-agent context budget (bytes). Raw fixtures larger than this are
/// truncated in the control arm — the regime where naive sessions lose data.
pub const CONTROL_BUDGET: usize = 2000;

/// Accurate token count via the offline o200k_base BPE (replaces the old bytes/4
/// heuristic), so the control/treatment token figures reflect real tokenization.
pub fn est_tokens(text: &str) -> usize {
    lens::obs::count_tokens(text)
}

pub fn accuracy_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/accuracy")
}

/// Default model id: a current small-but-capable model. Override with
/// `LENS_BENCH_MODEL`.
pub fn default_model() -> String {
    std::env::var("LENS_BENCH_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string())
}

/// Default model for the opencode agentic backend (`provider/model`).
pub fn default_opencode_model() -> String {
    std::env::var("LENS_BENCH_MODEL").unwrap_or_else(|_| "xai/grok-4.5".to_string())
}

// --- Task spec --------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct Treatment {
    pub language: Option<String>,
    pub script: Option<String>,
    pub queries: Option<Vec<String>>,
    pub graph_op: Option<String>,
    pub name: Option<String>,
    pub kind: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Skeleton: path to a source file to reduce to signatures + nesting.
    pub skeleton: Option<String>,
    /// Skeleton (L48): when true, the skeleton treatment is built with
    /// `with_lines: true` so definition headers carry `L{n}: ` line-number
    /// citations. Defaults to false, so every existing `skeleton` task is
    /// unaffected.
    #[serde(default)]
    pub with_lines: bool,
    /// Find: top-K budget for the `find` graph_op (how many ranked matches survive
    /// before their neighbors are pulled in). Defaults to 3.
    pub limit: Option<usize>,
    /// RRF fusion (L43): when true, the `queries` search is run through
    /// `Index::search_fused` with a per-file graph-importance rank built the
    /// same way as `Forge::file_ranks` (server.rs) / gate C20, instead of plain
    /// `Index::search`. `LENS_RRF` (read per call by `search_fused`) then
    /// toggles fusion on/off for the same task. Defaults to false, so every
    /// existing `queries` task is unaffected.
    #[serde(default)]
    pub graph_fused: bool,
    /// Overview focus (L40): the personalization query for an `overview` task.
    /// `overview_seed` seeds every node whose name matches a query token (+10), so
    /// a globally-unimportant symbol is lifted into a tight budget. Defaults to
    /// `None`, so every existing overview task keeps the global map.
    #[serde(default)]
    pub query: Option<String>,
    /// Overview focus (L40): per-task token budget for the `overview` treatment, so
    /// the focus flip is observable with a compact fixture. Defaults to `None` ==
    /// 2000 (the `lens_overview` tool default), unchanged for every existing task.
    #[serde(default)]
    pub overview_budget: Option<usize>,
    /// Neighbors: hops outward from the resolved node. Defaults to 1, the
    /// `lens_graph` default.
    pub depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct Task {
    pub id: String,
    pub prompt: String,
    pub fixtures: Vec<String>,
    pub ground_truth: Value,
    pub check: String,
    #[serde(default)]
    pub tolerance: Option<f64>,
    pub primary_mechanism: String,
    /// The lens MCP function this task primarily exercises (e.g.
    /// `"lens_search"`), tagged on every `real_agentic_*` task. Powers the
    /// per-function aggregation in `function_report`; absent (`None`) for the
    /// pre-agentic mechanism tasks, which don't map to a single agent-chosen
    /// tool.
    #[serde(default)]
    pub lens_fn: Option<String>,
    /// Substrings that must be present in a context for the answer to be
    /// derivable (used by the mock oracle; see `mock_answer`).
    pub evidence: Vec<String>,
    pub treatment: Treatment,
}

/// Load and sort all task specs.
pub fn load_tasks() -> anyhow::Result<Vec<Task>> {
    let dir = accuracy_root().join("tasks");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    paths.sort();
    let mut tasks = Vec::new();
    for p in paths {
        let raw = std::fs::read_to_string(&p)?;
        let task: Task = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", p.display()))?;
        tasks.push(task);
    }
    Ok(tasks)
}

// --- Frozen task sets --------------------------------------------------------

/// Resolve a `LENS_BENCH_SET` value to a file path: absolute as given, else
/// relative to the accuracy root, so `sets/0.10.json` resolves from anywhere.
pub fn resolve_set_path(raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        accuracy_root().join(raw)
    }
}

/// The set label stamped into the output JSON + report header: the set file's
/// stem (`sets/0.10.json` -> `0.10`), so a version's numbers name their set.
pub fn set_label(raw: &str) -> String {
    Path::new(raw)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| raw.to_string())
}

/// Load a frozen task set: a JSON array of task ids (`["0060_...", ...]`).
pub fn load_task_set(raw: &str) -> anyhow::Result<Vec<String>> {
    let path = resolve_set_path(raw);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading task set {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parsing task set {}: {e}", path.display()))
}

/// Apply the two composable task filters as an INTERSECTION: `set` (exact ids
/// from a `LENS_BENCH_SET` file) then `only` (the `LENS_BENCH_ONLY` substring on
/// id or mechanism). Either may be absent; when both are present a task must
/// satisfy BOTH, so a frozen set can still be re-run one mechanism at a time
/// rather than one filter replacing the other.
pub fn filter_tasks(mut tasks: Vec<Task>, set: Option<&[String]>, only: Option<&str>) -> Vec<Task> {
    if let Some(set) = set {
        tasks.retain(|t| set.iter().any(|id| id == &t.id));
    }
    if let Some(only) = only.filter(|s| !s.is_empty()) {
        tasks.retain(|t| t.primary_mechanism.contains(only) || t.id.contains(only));
    }
    tasks
}

// --- Model ------------------------------------------------------------------

/// The agent. Both arms use the same model; only the context differs.
pub enum Model {
    /// Context-presence oracle: returns the ground truth iff every `evidence`
    /// token is present in the given context, else "UNKNOWN" per key. This is a
    /// stub that exercises scoring/plumbing without API calls — NOT a substitute
    /// for the real-model run.
    Mock,
    /// Real Anthropic call via `curl` (no SDK dependency).
    Anthropic(String),
    /// Real model driven through `claude-pty` — interactive Claude Code in a
    /// PTY, so the call bills against plan quota instead of the Agent SDK credit
    /// pool. Tools are disabled, so the model answers purely from the prompt
    /// (the control/treatment context), exactly like the API arm. The string is
    /// the `--model` passed to `claude-pty` (empty = the session default).
    ClaudePty(String),
    /// Real model driven through headless `claude -p --output-format json`. Same
    /// plan-quota billing and tools-off isolation as `ClaudePty`, but the answer
    /// arrives as structured JSON (`.result`) instead of a PTY screen scrape. The
    /// string is the `--model` passed to `claude` (empty = the session default).
    ClaudeHeadless(String),
    /// Real model driven through headless `claude -p` with **tools live**. Unlike
    /// every other backend the arms are not two prebuilt contexts: both are agent
    /// sessions over this repo that differ only in whether the lens MCP server is
    /// configured at all, so the measured delta is lens's marginal contribution in
    /// the setting lens actually runs in. The string is the `--model` passed to
    /// `claude` (empty = the session default).
    ClaudeAgentic(String),
    /// Same agentic A/B shape as [`ClaudeAgentic`], driven through headless
    /// `opencode run --format json` (bills the configured provider — typically
    /// xAI/Grok — instead of Claude Code plan quota). The string is the
    /// `provider/model` passed to `opencode -m` (e.g. `xai/grok-4.5`).
    OpenCodeAgentic(String),
}

impl Model {
    pub fn label(&self) -> String {
        match self {
            Model::Mock => "mock".to_string(),
            Model::Anthropic(m) => m.clone(),
            Model::ClaudePty(m) if m.is_empty() => "claude-pty".to_string(),
            Model::ClaudePty(m) => format!("{m} (via claude-pty)"),
            Model::ClaudeHeadless(m) if m.is_empty() => "claude-headless".to_string(),
            Model::ClaudeHeadless(m) => format!("{m} (via claude-headless)"),
            Model::ClaudeAgentic(m) if m.is_empty() => "claude-agentic".to_string(),
            Model::ClaudeAgentic(m) => format!("{m} (via claude-agentic)"),
            Model::OpenCodeAgentic(m) if m.is_empty() => "opencode-agentic".to_string(),
            Model::OpenCodeAgentic(m) => format!("{m} (via opencode-agentic)"),
        }
    }

}

// --- Arm execution ----------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmResult {
    pub correct: bool,
    pub tokens: usize,
    pub context_bytes: usize,
    pub answer: Value,
    /// Tool calls the agent made. Agentic arms only; the tools-off backends are
    /// handed their context and call nothing, so the field is skipped there and
    /// their serialized shape is unchanged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rounds: usize,
    /// Wall-clock milliseconds of the live session (the result envelope's
    /// `duration_ms`). Agentic arms only; zero and skipped elsewhere. Renamed
    /// on the wire to `duration_ms` — the third headline metric (time-to-
    /// answer) of the optimization-toolkit positioning.
    #[serde(rename = "duration_ms", default, skip_serializing_if = "is_zero")]
    pub millis: usize,
    /// Tool names in call order. Agentic arms only; empty and skipped elsewhere.
    /// This is what shows whether an arm organically reached a lens tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<String>,
    /// True when this is a lens-arm agentic run whose canary already proved the
    /// lens plumbing works, yet the session made ZERO organic `mcp__lens__*`
    /// calls — a genuine non-adoption, not broken config. Recorded (not dropped)
    /// so the per-model `adoption_rate` reflects it; a prior run instead refused
    /// and DROPPED these cells, which destroyed the adoption signal and biased
    /// the record toward lens-engaging tasks. Skipped when false, so every
    /// tools-off/baseline cell and every committed pre-canary result serializes
    /// byte-identically.
    #[serde(default, skip_serializing_if = "is_false")]
    pub adoption_miss: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// K-run fold for one arm. Only present when an arm ran more than once, so a
/// single-run result serializes exactly as it did before K-run existed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmStats {
    pub runs: usize,
    pub success_rate: f64,
    pub mean_tokens: f64,
    pub stddev_tokens: f64,
    pub mean_rounds: f64,
    pub stddev_rounds: f64,
    /// Wall-clock mean/spread in milliseconds. `#[serde(default)]` so results
    /// serialized before time was measured still deserialize (as 0).
    #[serde(default)]
    pub mean_millis: f64,
    #[serde(default)]
    pub stddev_millis: f64,
    /// Runs where the arm made at least one `mcp__lens__*` call — the organic
    /// lens-adoption count for this arm.
    #[serde(default)]
    pub lens_runs: usize,
}

/// One run's answer, correctness, and tool sequence, kept for every run K
/// (unlike `control`/`treatment` below, which keep only the first). Lets an
/// offline regrade recompute a folded record's `success_rate` against a
/// corrected ground truth, and lets a composed-shape judgment look at all K
/// sequences instead of just the first run's.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub answer: Value,
    pub correct: bool,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub mechanism: String,
    /// The arm's first run — the single-value view every pre-K-run consumer reads.
    pub control: ArmResult,
    pub treatment: ArmResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_stats: Option<ArmStats>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub treatment_stats: Option<ArmStats>,
    /// Every run's answer/correct/tools, in order. `#[serde(default)]` so
    /// results committed before this existed still deserialize (as empty).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub control_runs: Vec<RunRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub treatment_runs: Vec<RunRecord>,
}

/// Run both arms of one task `runs` times each. `runs = 1` reproduces the
/// single-shot behavior exactly, down to the serialized shape.
pub async fn run_task(task: &Task, model: &Model, runs: usize) -> anyhow::Result<TaskResult> {
    let runs = runs.max(1);
    let (mut control, mut treatment) = match model {
        Model::ClaudeAgentic(id) => agentic_arms(task, id, runs, AgenticHost::Claude)?,
        Model::OpenCodeAgentic(id) => agentic_arms(task, id, runs, AgenticHost::OpenCode)?,
        _ => context_arms(task, model, runs).await?,
    };
    let control_runs = to_run_records(&control);
    let treatment_runs = to_run_records(&treatment);
    Ok(TaskResult {
        id: task.id.clone(),
        mechanism: task.primary_mechanism.clone(),
        control_stats: fold_arm(&control),
        treatment_stats: fold_arm(&treatment),
        control_runs,
        treatment_runs,
        control: control.remove(0),
        treatment: treatment.remove(0),
    })
}

/// Every run's answer/correct/tools, in call order. `fold_arm` keeps only the
/// aggregate stats and `control`/`treatment` keep only the first run; this is
/// the per-run detail both discard.
fn to_run_records(runs: &[ArmResult]) -> Vec<RunRecord> {
    runs.iter()
        .map(|r| RunRecord {
            answer: r.answer.clone(),
            correct: r.correct,
            tools: r.tools.clone(),
        })
        .collect()
}

/// The classic two-context arms: one control/treatment context built once, then
/// answered `runs` times (rebuilding is deterministic, so it would only burn time).
async fn context_arms(
    task: &Task,
    model: &Model,
    runs: usize,
) -> anyhow::Result<(Vec<ArmResult>, Vec<ArmResult>)> {
    let control_ctx = build_control_context(task)?;
    let treatment_ctx = build_treatment_context(task).await?;
    let mut control = Vec::new();
    let mut treatment = Vec::new();
    for _ in 0..runs {
        control.push(run_arm(task, model, &control_ctx)?);
        treatment.push(run_arm(task, model, &treatment_ctx)?);
    }
    Ok((control, treatment))
}

/// Fold `runs` repetitions of one arm into its rate/mean/stddev. `None` for a
/// single run: there is no variance to report, and the caller's single-value
/// fields already say everything.
fn fold_arm(runs: &[ArmResult]) -> Option<ArmStats> {
    if runs.len() < 2 {
        return None;
    }
    let successes: Vec<f64> = runs.iter().map(|r| r.correct as u8 as f64).collect();
    let tokens: Vec<f64> = runs.iter().map(|r| r.tokens as f64).collect();
    let rounds: Vec<f64> = runs.iter().map(|r| r.rounds as f64).collect();
    let millis: Vec<f64> = runs.iter().map(|r| r.millis as f64).collect();
    Some(ArmStats {
        runs: runs.len(),
        success_rate: mean(&successes),
        mean_tokens: mean(&tokens),
        stddev_tokens: stddev(&tokens),
        mean_rounds: mean(&rounds),
        stddev_rounds: stddev(&rounds),
        mean_millis: mean(&millis),
        stddev_millis: stddev(&millis),
        lens_runs: runs
            .iter()
            .filter(|r| r.tools.iter().any(|t| is_lens_tool_name(t)))
            .count(),
    })
}

fn mean(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.iter().sum::<f64>() / xs.len() as f64
}

/// Population standard deviation. Fewer than two samples have no spread, so
/// they report 0.0 rather than NaN.
pub fn stddev(xs: &[f64]) -> f64 {
    if xs.len() < 2 {
        return 0.0;
    }
    let m = mean(xs);
    (xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / xs.len() as f64).sqrt()
}

/// Standard deviation of a sum of independent per-task draws: variances add, so
/// summing the stddevs themselves would overstate the spread.
fn stddev_of_sum(sds: impl Iterator<Item = f64>) -> f64 {
    sds.map(|s| s * s).sum::<f64>().sqrt()
}

fn run_arm(task: &Task, model: &Model, context: &str) -> anyhow::Result<ArmResult> {
    let answer = match model {
        Model::Mock => mock_answer(context, &task.evidence, &task.ground_truth),
        Model::Anthropic(id) => {
            let user = format_user(context, &task.prompt, &task.ground_truth);
            let raw = call_anthropic(id, SYSTEM_PROMPT, &user)
                .map_err(|e| anyhow::anyhow!("anthropic call failed: {e}"))?;
            extract_json(&raw)
        }
        Model::ClaudePty(model) => {
            let user = format_user(context, &task.prompt, &task.ground_truth);
            let raw = call_claude_pty(model, SYSTEM_PROMPT, &user)
                .map_err(|e| anyhow::anyhow!("claude-pty call failed: {e}"))?;
            extract_json(&raw)
        }
        Model::ClaudeHeadless(model) => {
            let user = format_user(context, &task.prompt, &task.ground_truth);
            let raw = call_claude_headless(model, SYSTEM_PROMPT, &user)
                .map_err(|e| anyhow::anyhow!("claude headless call failed: {e}"))?;
            extract_json(&raw)
        }
        // `run_task` routes the agentic backend to `agentic_arms` before this
        // point: its arms explore the repo, they don't answer from a context.
        Model::ClaudeAgentic(_) | Model::OpenCodeAgentic(_) => {
            unreachable!("agentic arms do not run from a prebuilt context")
        }
    };
    let correct = score(&answer, &task.ground_truth, &task.check, task.tolerance);
    Ok(ArmResult {
        correct,
        tokens: est_tokens(context),
        context_bytes: context.len(),
        answer,
        rounds: 0, // handed its context; it calls nothing
        millis: 0,
        tools: vec![],
        adoption_miss: false, // tools-off arm; adoption is an agentic concept
    })
}

// --- Context construction ---------------------------------------------------

/// Control: concatenated raw fixture bytes, capped at CONTROL_BUDGET.
pub fn build_control_context(task: &Task) -> anyhow::Result<String> {
    let mut s = String::new();
    for f in &task.fixtures {
        let p = accuracy_root().join(f);
        if p.is_dir() {
            let mut files: Vec<PathBuf> = walkdir::WalkDir::new(&p)
                .into_iter()
                .flatten()
                .filter(|e| e.file_type().is_file())
                .map(|e| e.into_path())
                .collect();
            files.sort();
            for file in files {
                if let Ok(content) = std::fs::read_to_string(&file) {
                    let name = file.file_name().unwrap_or_default().to_string_lossy();
                    s.push_str(&format!("===== {name} =====\n{content}\n"));
                }
            }
        } else if let Ok(content) = std::fs::read_to_string(&p) {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            s.push_str(&format!("===== {name} =====\n{content}\n"));
        }
    }
    Ok(truncate_bytes(&s, CONTROL_BUDGET))
}

/// Treatment: the compact output of the lens tool the task names.
pub async fn build_treatment_context(task: &Task) -> anyhow::Result<String> {
    let t = &task.treatment;
    // Darkroom.
    if let Some(script) = &t.script {
        let dir = accuracy_root();
        let data = tempfile::tempdir()?;
        let store = Store::open(&data.path().join(".lens"))?;
        let req = ExecuteRequest {
            path: None,
            language: t.language.clone().unwrap_or_else(|| "bash".to_string()),
            code: script.clone(),
            timeout_secs: 30,
            stdin: None,
        };
        let resp = darkroom::run(req, &dir, &store, 8192)
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut out = resp.stdout;
        if resp.exit_code != 0 && !resp.stderr.is_empty() {
            out.push_str(&format!("\n[stderr] {}", resp.stderr));
        }
        return Ok(out);
    }
    // Search.
    if let Some(queries) = &t.queries {
        let data = tempfile::tempdir()?;
        let mut index = Index::open(data.path())?;
        let fixture = accuracy_root().join(&task.fixtures[0]);
        if t.graph_fused {
            index = index.with_repo_root(&fixture);
        }
        for f in &task.fixtures {
            index.index_path(&accuracy_root().join(f), true)?;
        }
        let resp = if t.graph_fused {
            // Per-file graph-importance rank, built exactly like `Forge::file_ranks`
            // (server.rs) / gate C20 (benchmarks/changes/run_changes.rs): sum
            // `Graph::importance()` over each file's nodes, rank desc, ties by
            // path asc.
            let graph = discovery::discover(&fixture, None)?.graph;
            let importance = graph.importance();
            let mut per_file: std::collections::HashMap<&str, f64> = std::collections::HashMap::new();
            for node in &graph.nodes {
                if let Some(score) = importance.get(&node.id) {
                    *per_file.entry(node.file.as_str()).or_insert(0.0) += *score;
                }
            }
            let mut files: Vec<(&str, f64)> = per_file.into_iter().collect();
            files.sort_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.0.cmp(b.0))
            });
            let file_ranks: std::collections::HashMap<String, usize> = files
                .into_iter()
                .enumerate()
                .map(|(rank, (path, _score))| (path.to_string(), rank))
                .collect();
            index.search_fused(queries, 5, &file_ranks)?
        } else {
            index.search(queries, 5)?
        };
        return Ok(serde_json::to_string_pretty(&resp)?);
    }
    // Discovery.
    if let Some(op) = &t.graph_op {
        let repo = accuracy_root().join(&task.fixtures[0]);
        let outcome = discovery::discover(&repo, None)?;
        let json = match op.as_str() {
            "query" => {
                let view = gquery::query(
                    &outcome.graph,
                    t.name.as_deref().unwrap_or(""),
                    t.kind.as_deref(),
                    20,
                    &[],
                );
                serde_json::to_string_pretty(&view)?
            }
            "path" => {
                let resp = gquery::path(
                    &outcome.graph,
                    t.from.as_deref().unwrap_or(""),
                    t.to.as_deref().unwrap_or(""),
                );
                serde_json::to_string_pretty(&resp)?
            }
            // The token-budgeted repomap. L40 focus A/B lever, mirroring the `find`
            // arm's LENS_FIND_RANK: the personalization seed is applied UNLESS
            // LENS_OVERVIEW_FOCUS=0, so the SAME task runs focus-on (treatment) vs
            // focus-off across two harness runs, isolating the personalized-overview
            // win. Focus-off uses an empty seed == the pre-L40 static render by
            // construction. Budget defaults to 2000 (the lens_overview tool default).
            "overview" => {
                let budget = t.overview_budget.unwrap_or(2000);
                let seed = if std::env::var("LENS_OVERVIEW_FOCUS").as_deref() == Ok("0") {
                    std::collections::HashMap::new()
                } else {
                    gquery::overview_seed(&outcome.graph, &[], t.query.as_deref())
                };
                gquery::overview(&outcome.graph, budget, &seed)
            }
            // Natural-language find, re-ranked per `LENS_FIND_RANK` (L36 A/B lever).
            // The treatment IS the lens improvement, so it defaults to the
            // personalized-PR ranking; the A/B control sets `raw` (current
            // production lexical) and the third arm sets `blend` (lexical-primary,
            // PR tie-break).
            "find" => {
                let rank = match std::env::var("LENS_FIND_RANK").as_deref() {
                    Ok("raw") => gquery::FindRank::Raw,
                    Ok("blend") => gquery::FindRank::Blend,
                    _ => gquery::FindRank::Personalized,
                };
                let view = gquery::find_ranked(
                    &outcome.graph,
                    t.name.as_deref().unwrap_or(""),
                    t.limit.unwrap_or(3),
                    rank,
                );
                serde_json::to_string_pretty(&view)?
            }
            // Who-calls-X / what-does-X-call, the `lens_graph` path. `lens_graph`
            // takes the node id a prior `lens_symbol` call handed the model, so
            // the name -> id resolution a task spec needs happens here.
            "neighbors" => {
                let name = t.name.as_deref().unwrap_or("");
                let id = resolve_node_id(&outcome.graph, name, t.kind.as_deref())
                    .ok_or_else(|| anyhow::anyhow!("neighbors: no node matching '{name}'"))?;
                let view = gquery::neighbors(&outcome.graph, &id, t.depth.unwrap_or(1));
                neighbors_context(&view, &id)?
            }
            other => return Err(anyhow::anyhow!("unknown graph_op '{other}'")),
        };
        return Ok(json);
    }
    // Skeleton: signatures + nesting of one file, bodies elided.
    if let Some(path) = &t.skeleton {
        let p = accuracy_root().join(path);
        let content = std::fs::read_to_string(&p)?;
        let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
        let spec = discovery::extract::spec_for_extension(ext)
            .ok_or_else(|| anyhow::anyhow!("no language spec for {}", p.display()))?;
        let skel = discovery::skeleton::skeletonize(&content, &spec, None, t.with_lines)
            .ok_or_else(|| anyhow::anyhow!("could not skeletonize {}", p.display()))?;
        return Ok(skel);
    }
    Err(anyhow::anyhow!("task {} has no treatment spec", task.id))
}

/// Resolve a symbol token to a node id the way `query::path` does — exact id,
/// then exact name, then first substring hit. `query::resolve` is private, so a
/// name-addressed `neighbors` task needs its own copy of the rule.
fn resolve_node_id(graph: &Graph, token: &str, kind: Option<&str>) -> Option<String> {
    if graph.node(token).is_some() {
        return Some(token.to_string());
    }
    let matches = graph.find_by_name(token, kind);
    matches
        .iter()
        .find(|n| n.name == token)
        .or_else(|| matches.first())
        .map(|n| n.id.clone())
}

/// `lens_graph`'s neighborhood, joined into named callers and callees. The raw
/// view's edges carry opaque blake3 ids, so who-calls-X is unanswerable from it
/// without this join. `via` keeps the edge kind visible, so a containing module
/// (a `contains` edge) is never mistaken for a caller.
fn neighbors_context(view: &GraphView, target_id: &str) -> anyhow::Result<String> {
    let by_id: std::collections::HashMap<&str, &NodeView> =
        view.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let callers: Vec<Value> = view
        .edges
        .iter()
        .filter(|e| e.to == target_id)
        .filter_map(|e| by_id.get(e.from.as_str()).map(|n| linked(n, &e.kind)))
        .collect();
    let callees: Vec<Value> = view
        .edges
        .iter()
        .filter(|e| e.from == target_id)
        .filter_map(|e| by_id.get(e.to.as_str()).map(|n| linked(n, &e.kind)))
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "target": by_id.get(target_id),
        "callers": callers,
        "callees": callees,
    }))?)
}

fn linked(n: &NodeView, via: &str) -> Value {
    json!({ "via": via, "name": n.name, "kind": n.kind, "file": n.file, "line": n.line })
}

fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut idx = max;
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    s[..idx].to_string()
}

// --- The two model implementations -----------------------------------------

const SYSTEM_PROMPT: &str = "You are a precise data-extraction assistant. Answer strictly from the provided context. Respond with a single minified JSON object and nothing else — no prose, no code fences.";

fn ground_truth_keys(ground_truth: &Value) -> Vec<String> {
    ground_truth
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

fn format_user(context: &str, prompt: &str, ground_truth: &Value) -> String {
    let keys = ground_truth_keys(ground_truth);
    format!(
        "Context:\n{context}\n\nQuestion: {prompt}\n\nRespond with ONLY a JSON object with exactly these keys: {keys:?}."
    )
}

/// Mock oracle (see `Model::Mock`).
pub fn mock_answer(context: &str, evidence: &[String], ground_truth: &Value) -> Value {
    let lc = context.to_ascii_lowercase();
    let derivable = evidence
        .iter()
        .all(|e| lc.contains(&e.to_ascii_lowercase()));
    if derivable {
        ground_truth.clone()
    } else {
        let mut m = Map::new();
        if let Some(obj) = ground_truth.as_object() {
            for k in obj.keys() {
                m.insert(k.clone(), json!("UNKNOWN"));
            }
        }
        Value::Object(m)
    }
}

/// Real Anthropic Messages API call via `curl`.
fn call_anthropic(model: &str, system: &str, user: &str) -> Result<String, String> {
    let key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY not set")?;
    let body = json!({
        "model": model,
        "max_tokens": 1024,
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    })
    .to_string();

    let mut child = Command::new("curl")
        .args([
            "-sS",
            "https://api.anthropic.com/v1/messages",
            "-H",
            "content-type: application/json",
            "-H",
            "anthropic-version: 2023-06-01",
            "-H",
            &format!("x-api-key: {key}"),
            "--data-binary",
            "@-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning curl: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(body.as_bytes())
        .map_err(|e| format!("writing body: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting on curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let resp: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("parsing response: {e}"))?;
    if let Some(err) = resp.get("error") {
        return Err(format!("api error: {err}"));
    }
    resp["content"][0]["text"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("unexpected response shape: {resp}"))
}

/// Drive a real model through `claude-pty` (interactive Claude Code in a PTY).
/// Bills against plan quota, not the Agent SDK credit pool.
///
/// Tools are disabled (`--allowed-tools ""`) so the model answers purely from
/// the prompt — same isolation as the `curl` Anthropic arm — which also means
/// the already-trusted project dir is a safe working dir (no file access, so no
/// need for `--dangerously-skip-permissions`). claude-pty returns a screen
/// scrape that echoes the prompt; the answer is the trailing JSON object, so we
/// pull the **last** balanced `{...}` and hand that to the scorer.
fn call_claude_pty(model: &str, system: &str, user: &str) -> Result<String, String> {
    // claude-pty takes a single stdin prompt; fold the system instruction in.
    let prompt = format!("{system}\n\n{user}");
    // Retry: heavy back-to-back sessions occasionally get the child SIGKILLed
    // under memory pressure; a transient kill succeeds on a second attempt.
    let mut last_err = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        match claude_pty_attempt(&prompt, model) {
            Ok(obj) => return Ok(obj),
            Err(e) => {
                eprintln!("  claude-pty attempt {} failed: {e}", attempt + 1);
                last_err = e;
            }
        }
    }
    Err(last_err)
}

fn claude_pty_attempt(prompt: &str, model: &str) -> Result<String, String> {
    let workdir = env!("CARGO_MANIFEST_DIR"); // trusted; tools are off regardless

    let mut cmd = Command::new("claude-pty");
    cmd.args(["--working-dir", workdir])
        .args(["--allowed-tools", ""]) // disable all tools — answer from prompt only
        .args(["--effort", "low"]) // a JSON-extraction answer needs no deep exploration
        .args(["--timeout", "120"]);
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning claude-pty: {e}"))?;
    child
        .stdin
        .take()
        .ok_or("no stdin")?
        .write_all(prompt.as_bytes())
        .map_err(|e| format!("writing prompt: {e}"))?;
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting on claude-pty: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    // claude-pty exits non-zero on a salvaged hard-timeout but may still have
    // captured the answer; only fail if we can't find a JSON object at all.
    match last_json_object(&stdout) {
        Some(obj) => Ok(obj),
        None => Err(format!(
            "claude-pty produced no JSON object (status {}, stderr: {})",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )),
    }
}

/// Drive a real model through headless `claude -p --output-format json`. Same
/// plan-quota billing and tools-off isolation as `call_claude_pty`, but the
/// answer comes back as structured JSON, so no screen scrape.
fn call_claude_headless(model: &str, system: &str, user: &str) -> Result<String, String> {
    let prompt = format!("{system}\n\n{user}");
    let mut last_err = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        match claude_headless_attempt(&prompt, model) {
            Ok(s) => return Ok(s),
            Err(e) => {
                eprintln!("  claude headless attempt {} failed: {e}", attempt + 1);
                last_err = e;
            }
        }
    }
    Err(last_err)
}

/// One headless call. `--output-format json` returns an envelope whose `.result`
/// is the model's final text (carrying the answer JSON). `perl alarm` is a
/// portable 120s wall-clock bound (headless has no `--timeout`, macOS no `timeout`).
fn claude_headless_attempt(prompt: &str, model: &str) -> Result<String, String> {
    let workdir = env!("CARGO_MANIFEST_DIR"); // trusted; tools are off regardless
    // Reasoning effort, default low (short JSON answers); override with LENS_BENCH_EFFORT.
    let effort = std::env::var("LENS_BENCH_EFFORT").unwrap_or_else(|_| "low".to_string());
    let mut cmd = Command::new("perl");
    cmd.current_dir(workdir)
        .args(["-e", "alarm shift; exec @ARGV", "120"])
        .arg("claude")
        .arg("-p")
        .arg(prompt)
        .args(["--output-format", "json"])
        .args(["--allowedTools", ""]) // disable all tools — answer from prompt only
        .args(["--effort", effort.as_str()]);
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning claude: {e}"))?
        .wait_with_output()
        .map_err(|e| format!("waiting on claude: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let resp: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "claude headless non-JSON stdout (status {}, err {e}, stderr: {})",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })?;
    if resp.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        return Err(format!(
            "claude headless is_error: {}",
            resp.get("result").and_then(Value::as_str).unwrap_or("?")
        ));
    }
    resp.get("result")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "claude headless missing .result: {}",
                stdout.chars().take(300).collect::<String>()
            )
        })
}

// --- Agentic backend (tools live, lens MCP wired in) ------------------------

/// Builtin file tools, permitted to both arms.
const BASELINE_TOOLS: &str = "Read Grep Glob Bash";
/// The lens arm additionally permits the lens MCP tools. This is a *permission*
/// list, not an isolation boundary: `--allowedTools` was measured NOT to stop a
/// baseline session from reaching an MCP server it can see (it found lens via
/// `ToolSearch` and called it successfully). Isolation is structural — see
/// `arm_isolation`.
const LENS_TOOLS: &str = "Read Grep Glob Bash mcp__lens";

/// Proof that lens's SessionStart guide reached a session: the lens arm must see
/// it (it ships with `lens setup`), the baseline must not.
const GUIDE_SENTINEL: &str = "<context_window_protection>";

const AGENTIC_SYSTEM_PROMPT: &str = "You are a precise code-analysis assistant working in a real repository. Investigate with the tools available until you can answer. Respond with a single minified JSON object and nothing else — no prose, no code fences.";

/// The task's fixtures as repo-root-relative paths: the agentic arms run from the
/// repo root, while `fixtures` are written relative to `accuracy_root()`.
fn fixture_scope(task: &Task) -> Vec<String> {
    let root = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR"))
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    task.fixtures
        .iter()
        .map(|f| {
            let joined = accuracy_root().join(f);
            let abs = std::fs::canonicalize(&joined).unwrap_or(joined);
            abs.strip_prefix(&root)
                .unwrap_or(&abs)
                .to_string_lossy()
                .to_string()
        })
        .collect()
}

/// The agentic arms get no prebuilt context: finding the evidence with the tools
/// they have IS the measurement. They do get the fixture scope, because every
/// other arm is implicitly scoped to it (the control sees only the fixture's
/// bytes; the treatment graph is built from the fixture) and the ground truth
/// only asserts what holds *inside* it — asked repo-wide, a correct answer scores
/// wrong.
fn format_agentic_user(task: &Task) -> String {
    let keys = ground_truth_keys(&task.ground_truth);
    let scope = fixture_scope(task).join(", ");
    format!(
        "{AGENTIC_SYSTEM_PROMPT}\n\nScope: answer only about code under `{scope}` in this repository. Ignore matches outside that path.\n\nQuestion: {}\n\nRespond with ONLY a JSON object with exactly these keys: {keys:?}.",
        task.prompt
    )
}

/// Which agent host drives the live agentic A/B.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AgenticHost {
    Claude,
    OpenCode,
}

/// Live A/B of two configs a user could actually install: **vanilla host** vs
/// **lens as `lens setup` installs it**. The adoption layer (SessionStart guide,
/// nudge/deny rails) ships with lens, so it belongs to the lens arm rather than
/// being equalized away; equalizing it would amputate the thing being measured.
fn agentic_arms(
    task: &Task,
    model: &str,
    runs: usize,
    host: AgenticHost,
) -> anyhow::Result<(Vec<ArmResult>, Vec<ArmResult>)> {
    let iso = arm_isolation(host)?;
    let mut control = Vec::new();
    let mut treatment = Vec::new();
    for _ in 0..runs {
        control.push(run_agentic_arm(task, model, &iso.baseline(), host)?);
        treatment.push(run_agentic_arm(task, model, &iso.lens_arm(), host)?);
    }
    Ok((control, treatment))
}

/// One side of the A/B as it reaches the agent host.
struct ArmSpec<'a> {
    allowed_tools: &'a str,
    /// Claude: `--mcp-config` JSON. OpenCode: full `OPENCODE_CONFIG` JSON.
    mcp_config: &'a Path,
    /// Claude only: `--settings` JSON. OpenCode ignores this path.
    settings: &'a Path,
    /// Whether lens's SessionStart guide must reach this arm. A lens arm without
    /// it means routing silently failed to take; a baseline with it means lens
    /// leaked in. Either way the arm measures the wrong thing, so it fails loudly.
    expects_guide: bool,
}

/// What separates the arms, on disk. Held together because the files must all
/// outlive the sessions that read them.
struct ArmIsolation {
    baseline_mcp: tempfile::NamedTempFile,
    baseline_settings: tempfile::NamedTempFile,
    lens_mcp: tempfile::NamedTempFile,
    lens_settings: tempfile::NamedTempFile,
}

impl ArmIsolation {
    fn baseline(&self) -> ArmSpec<'_> {
        ArmSpec {
            allowed_tools: BASELINE_TOOLS,
            mcp_config: self.baseline_mcp.path(),
            settings: self.baseline_settings.path(),
            expects_guide: false,
        }
    }

    fn lens_arm(&self) -> ArmSpec<'_> {
        ArmSpec {
            allowed_tools: LENS_TOOLS,
            mcp_config: self.lens_mcp.path(),
            settings: self.lens_settings.path(),
            expects_guide: true,
        }
    }
}

/// `--mcp-config` always points at `lens_release_bin` (this run's own build),
/// not whatever binary a prior `lens session install` happened to register
/// globally. `--settings` merges onto the ambient config rather than
/// replacing it (confirmed against anthropics/claude-code#11392: a hook
/// array from `--settings` is additive, not a replacement), so it cannot
/// *un*-register an already-installed hook. `lens_settings_json` does not
/// need it to: it carries its own copy of the five lifecycle hooks, pointed
/// at `lens_release_bin` directly, so the lens arm is self-consistent (MCP
/// server and hooks both on this run's fresh build) with no dependence on
/// whatever is or isn't installed globally. Any stale ambient lens hooks
/// merge in and fire too, but harmlessly: `route_inner`'s tool match has no
/// `mcp__lens__*` arm (old binary or new), so an MCP tool call is already an
/// unconditional `Decision::Passthrough` regardless of whether the hook
/// recognizes the name, and a stale SessionStart guide just adds a second,
/// superseded copy of the injected text alongside the correct one (Claude
/// Code runs every matching hook and concatenates their `additionalContext`).
/// A mismatch here is exactly what voided the 560e862 acceptance run (hooks
/// and MCP disagreeing on the tool set produced "No matching deferred tools
/// found" and zero organic `mcp__lens__*` calls, yet the validity gate at the
/// time only checked that the guide fired); `validate_arm_run` is the
/// backstop that now catches a recurrence instead of scoring it as a loss.
fn arm_isolation(host: AgenticHost) -> anyhow::Result<ArmIsolation> {
    let bin = lens_release_bin();
    eprintln!(
        "agentic bench: host={host:?} lens bin={}",
        bin.display()
    );
    match host {
        AgenticHost::Claude => {
            if !supports_strict_mcp_config() {
                return Err(anyhow::anyhow!(
                    "this `claude` CLI has no --strict-mcp-config, so the baseline arm cannot be \
                     isolated from the ambient lens MCP server; refusing to run an invalid A/B"
                ));
            }
            Ok(ArmIsolation {
                baseline_mcp: write_temp_json(&json!({ "mcpServers": {} }))?,
                baseline_settings: write_temp_json(&baseline_settings_json())?,
                lens_mcp: write_temp_json(&mcp_config_json(&bin))?,
                lens_settings: write_temp_json(&lens_settings_json())?,
            })
        }
        AgenticHost::OpenCode => {
            // OPENCODE_CONFIG replaces the ambient config for the child. Baseline
            // explicitly disables the ambient `lens` MCP entry; lens arm points at
            // this run's release binary with LENS_HOST=opencode.
            Ok(ArmIsolation {
                baseline_mcp: write_temp_json(&opencode_baseline_config_json())?,
                baseline_settings: write_temp_json(&json!({}))?,
                lens_mcp: write_temp_json(&opencode_lens_config_json(&bin))?,
                lens_settings: write_temp_json(&json!({}))?,
            })
        }
    }
}

/// OpenCode baseline: no lens MCP (ambient `lens` entry forced off).
fn opencode_baseline_config_json() -> Value {
    json!({
        "mcp": { "lens": { "enabled": false } },
        "permission": { "*": "allow" }
    })
}

/// OpenCode lens arm: MCP server = this run's release binary, host pinned.
fn opencode_lens_config_json(lens_bin: &Path) -> Value {
    json!({
        "mcp": {
            "lens": {
                "type": "local",
                "command": [lens_bin.to_string_lossy()],
                "enabled": true,
                "environment": {
                    "LENS_HOST": "opencode",
                    "LENS_ROUTING": "full"
                }
            }
        },
        "permission": { "*": "allow" }
    })
}

fn write_temp_json(v: &Value) -> anyhow::Result<tempfile::NamedTempFile> {
    let mut f = tempfile::NamedTempFile::new()?;
    f.write_all(serde_json::to_string(v)?.as_bytes())?;
    Ok(f)
}

/// MCP config pointing at the release lens binary with no subcommand — the same
/// server semantics `lens setup`'s `register_mcp` registers.
fn mcp_config_json(lens_bin: &Path) -> Value {
    json!({ "mcpServers": { "lens": { "command": lens_bin.to_string_lossy(), "args": [] } } })
}

/// Baseline = a machine where lens was never installed. Its hooks are registered
/// globally and cannot be un-merged (settings merge, so `{"hooks":{}}` is inert),
/// but `LENS_ROUTING=off` is a true no-op — "PreToolUse returns {}, SessionStart
/// unchanged" — so they fire and contribute nothing. Rails pinned off likewise.
/// This has to travel as an env-only `--settings` file because ambient settings
/// env beats the child's process env (the toolsel harness does the same).
fn baseline_settings_json() -> Value {
    let mut env = Map::new();
    env.insert("LENS_ROUTING".to_string(), json!("off"));
    for flag in REROUTE_RAIL_FLAGS {
        env.insert((*flag).to_string(), json!("0"));
    }
    json!({ "env": env })
}

/// The five lifecycle hook events `lens session install` registers. Keep in
/// sync with `src/session/install.rs`'s `EVENTS`.
const HOOK_EVENTS: [&str; 5] =
    ["PreToolUse", "PostToolUse", "UserPromptSubmit", "PreCompact", "SessionStart"];

/// lens arm = lens exactly as `lens setup` installs it: routing at its shipping
/// default. `full` is nudge + steer + wrap, which is what injects the SessionStart
/// guide and arms the deny rails. The 13 rails are deliberately left unset: they
/// are default-ON kill-switches, so unset IS the shipping default and pinning them
/// to "0" would disable the adoption layer under test.
///
/// Also carries its own copy of the five lifecycle hooks, the same shape
/// `install()` in `src/session/install.rs` writes, pointed at `lens_release_bin`
/// instead of whatever is installed globally, so the arm's hooks and its
/// `--mcp-config` server agree on the tool set (see `arm_isolation`).
fn lens_settings_json() -> Value {
    let bin = lens_release_bin().to_string_lossy().to_string();
    let hooks: Map<String, Value> = HOOK_EVENTS
        .iter()
        .map(|event| {
            let group = json!([{
                "matcher": "",
                "hooks": [{ "type": "command", "command": format!("\"{bin}\" hook claude {event}") }]
            }]);
            (event.to_string(), group)
        })
        .collect();
    json!({ "env": { "LENS_ROUTING": "full" }, "hooks": hooks })
}

pub fn lens_release_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/release/lens")
}

/// Whether the installed `claude` CLI recognizes `--strict-mcp-config` (checked
/// once per process; older CLIs lack it).
fn supports_strict_mcp_config() -> bool {
    static CACHE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        Command::new("claude")
            .args(["-p", "--help"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("--strict-mcp-config"))
            .unwrap_or(false)
    })
}

/// All 13 reroute-rail nudge/deny flags, default-ON kill-switches (`=0` disables).
/// Keep in sync with the flag matrix in `src/routing/reroute/mod.rs`.
const REROUTE_RAIL_FLAGS: &[&str] = &[
    "LENS_GREP_SCOPE_DENY",
    "LENS_GREP_SYMBOL_DENY",
    "LENS_GREP_SYMBOL_NUDGE",
    "LENS_READ_SKELETON_DENY",
    "LENS_READ_SKELETON_NUDGE",
    "LENS_GREP_AST_NUDGE",
    "LENS_GREP_AST_DENY",
    "LENS_READ_OVERVIEW_NUDGE",
    "LENS_READ_OVERVIEW_DENY",
    "LENS_BASH_AGG_NUDGE",
    "LENS_BASH_AGG_DENY",
    "LENS_EDIT_LINKS_NUDGE",
    "LENS_EDIT_LINKS_DENY",
];

/// One arm, retried once (a transient kill/timeout succeeds on the second try).
fn run_agentic_arm(
    task: &Task,
    model: &str,
    arm: &ArmSpec,
    host: AgenticHost,
) -> anyhow::Result<ArmResult> {
    let prompt = format_agentic_user(task);
    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let attempt_result = match host {
            AgenticHost::Claude => claude_agentic_attempt(&prompt, model, arm),
            AgenticHost::OpenCode => opencode_agentic_attempt(&prompt, model, arm),
        };
        match attempt_result {
            Ok(run) => {
                // A misconfigured arm is not worth retrying, and silently measuring
                // it is how the first two builds shipped an invalid A/B. Post-canary
                // a zero-lens lens-arm run is no longer fatal: it scores and carries
                // `adoption_miss`, which the gate returns here.
                let adoption_miss = match validate_arm_run(&run, arm) {
                    Ok(miss) => miss,
                    Err(msg) => return Err(anyhow::anyhow!(msg)),
                };
                if run.hit_turn_cap {
                    eprintln!(
                        "  WARNING: agentic [{}] hit the CLI's own turn cap (no --max-turns is \
                         passed); scored as a failed run (no answer inside the turn budget)",
                        arm.allowed_tools
                    );
                }
                let answer = extract_json(&run.answer);
                return Ok(ArmResult {
                    correct: score(&answer, &task.ground_truth, &task.check, task.tolerance),
                    tokens: run.tokens,
                    // The only context we hand an agentic arm is the question; what
                    // it pulls in beyond that is its own doing, and `tokens` is what
                    // measures it.
                    context_bytes: prompt.len(),
                    answer,
                    rounds: run.rounds(),
                    millis: run.duration_ms,
                    tools: run.tools,
                    adoption_miss,
                });
            }
            Err(e) => {
                eprintln!(
                    "  agentic [{}] attempt {} failed: {e}",
                    arm.allowed_tools,
                    attempt + 1
                );
                last_err = e;
            }
        }
    }
    Err(anyhow::anyhow!(
        "agentic arm [{}]: {last_err}",
        arm.allowed_tools
    ))
}

/// What one agentic session measured.
struct AgenticRun {
    answer: String,
    /// Raw `tool_use` names in call order. `rounds` is just its length. Names
    /// alone are ATTEMPTS: an instructed model can emit a `mcp__lens__*` call
    /// in an arm whose config cannot serve it (`--strict-mcp-config`), so lens
    /// REACH is proven by `lens_call_succeeded`, not by these names.
    tools: Vec<String>,
    /// True iff at least one `mcp__lens__*` call got a NON-error `tool_result`
    /// back. The canary verdicts key on this: an attempted-but-failed lens call
    /// proves the baseline's isolation held (the plumbing refused the call),
    /// while only a SUCCEEDED call proves the lens arm's plumbing works.
    lens_call_succeeded: bool,
    tokens: usize,
    /// Times lens's SessionStart guide landed in this session.
    guide_injections: usize,
    /// True if the session was cut off by `--max-turns` rather than finishing. A
    /// deny rail costs the lens arm a round to recover, so the cap can silently
    /// depress its result.
    hit_turn_cap: bool,
    /// Wall-clock milliseconds from the result envelope's `duration_ms`.
    duration_ms: usize,
}

impl AgenticRun {
    fn rounds(&self) -> usize {
        self.tools.len()
    }
}

/// The validity gate, post-canary. Two failure modes remain fatal; the third —
/// a guide-fired lens arm that made ZERO organic `mcp__lens__*` calls — is no
/// longer dropped. The per-(model, arm) canary has already proven this arm's
/// lens plumbing reaches lens (the 560e862 "hooks/mcp-config binaries disagree"
/// bug would have failed the canary and aborted the suite), so a zero-lens
/// organic run is a genuine ADOPTION MISS: it SCORES and returns `true` here so
/// the caller records it, instead of being refused and silently deleted from the
/// record (which destroyed per-model adoption signal and biased the set toward
/// lens-engaging tasks). Still fatal: the SessionStart guide didn't fire the way
/// this arm expects (routing config didn't take), and a transcript that carried
/// no usage anywhere parses as tokens=0 (task 0079's lens cell did, across all
/// three runs) and would silently flatter the arm's token mean. Returns
/// `Ok(adoption_miss)`.
fn validate_arm_run(run: &AgenticRun, arm: &ArmSpec) -> Result<bool, String> {
    if (run.guide_injections > 0) != arm.expects_guide {
        return Err(format!(
            "arm [{}] expected lens guide={} but saw {} injection(s): its routing \
             config did not take, so the arm is measuring the wrong thing",
            arm.allowed_tools, arm.expects_guide, run.guide_injections
        ));
    }
    if run.tokens == 0 {
        return Err(format!(
            "arm [{}] transcript carried no usage anywhere (result envelope or \
             assistant messages): tokens=0 is a measurement failure, not a free \
             session; refusing to score it",
            arm.allowed_tools
        ));
    }
    let adoption_miss = arm.expects_guide && !run.tools.iter().any(|t| is_lens_tool_name(t));
    Ok(adoption_miss)
}

/// Claude emits `mcp__lens__lens_search`; opencode emits `lens_lens_search`
/// (server name + tool name). Both count as lens reach.
fn is_lens_tool_name(name: &str) -> bool {
    name.starts_with("mcp__lens__") || name.starts_with("lens_lens_")
}

// --- Canary gate + gated suite ----------------------------------------------

/// The canary prompt forces exactly one lens call, so a passing lens arm is
/// unambiguous and a broken one is loud. Run through the SAME plumbing as the
/// scored arm (`--mcp-config`, settings/hooks file, binary), it proves config
/// validity ONCE per (model, arm) before any task is scored. Names both the
/// Claude MCP spelling and the opencode server+tool spelling.
const CANARY_PROMPT: &str = "Use the lens search tool exactly once \
    (`mcp__lens__lens_search` or `lens_lens_search`), with its `queries` argument \
    set to [\"canary\"]. Then reply with only the minified JSON object \
    {\"canary\":\"ok\"} and nothing else — no prose, no code fences.";

/// Did the forced-lens session behave as this arm's config promises? The lens
/// arm must SUCCEED at a lens call (its plumbing works); the baseline arm must
/// have NO lens call succeed (its isolation holds). Keyed on `tool_result`
/// success, not `tool_use` names: the canary prompt is adversarial, so an
/// obedient baseline model routinely ATTEMPTS the instructed call — the
/// attempt then erroring is proof isolation held, not a leak. (The prior
/// name-based verdict scored those attempts as FAIL: 8 of 9 sonnet baseline
/// canaries aborted on 2026-07-21 with `--strict-mcp-config` verifiably in
/// force.) A pass either way means the arm measures the right thing, so an
/// organic zero-lens run on the lens arm can be trusted as real non-adoption
/// rather than silently-broken config.
fn canary_verdict(run: &AgenticRun, arm: &ArmSpec) -> bool {
    if arm.expects_guide {
        run.lens_call_succeeded
    } else {
        !run.lens_call_succeeded
    }
}

/// Run the canary for one arm config: a live forced-lens session through the
/// arm's exact plumbing, retried once for a transient kill. Returns the
/// pass/fail verdict; a transport failure that never yields a transcript is an
/// error (the config can't be proven either way, so the suite aborts).
fn run_canary(model: &str, arm: &ArmSpec, host: AgenticHost) -> anyhow::Result<bool> {
    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        let attempt_result = match host {
            AgenticHost::Claude => claude_agentic_attempt(CANARY_PROMPT, model, arm),
            AgenticHost::OpenCode => opencode_agentic_attempt(CANARY_PROMPT, model, arm),
        };
        match attempt_result {
            Ok(run) => return Ok(canary_verdict(&run, arm)),
            Err(e) => {
                eprintln!(
                    "  canary [{}] attempt {} failed: {e}",
                    arm.allowed_tools,
                    attempt + 1
                );
                last_err = e;
            }
        }
    }
    Err(anyhow::anyhow!(
        "canary session never produced a transcript: {last_err}"
    ))
}

/// Abort the suite loudly when the canary failed: a broken arm makes every cell
/// untrustworthy, so NO cell is scored at all (a prior run instead silently
/// dropped the zero-lens cells, which is exactly the failure this replaces).
/// Split out so the "no scored cells on canary fail" contract is unit-testable
/// with an injected verdict.
fn canary_gate(canary_ok: bool) -> anyhow::Result<()> {
    if !canary_ok {
        anyhow::bail!(
            "CANARY FAILED: an arm's forced-lens session did not behave as its config \
             promises (the lens arm must reach lens; the baseline must not). The plumbing \
             is broken, so refusing to score ANY cell — see the per-arm FAIL line above."
        );
    }
    Ok(())
}

/// Score the whole task list behind the canary gate: abort with NO results when
/// the canary failed, else run every task. Kept separate from the live canary so
/// the gate is testable with an injected verdict (a real canary needs a session).
async fn score_gated(
    tasks: &[Task],
    model: &Model,
    runs: usize,
    canary_ok: bool,
) -> anyhow::Result<Vec<TaskResult>> {
    canary_gate(canary_ok)?;
    let mut results = Vec::new();
    for task in tasks {
        match run_task(task, model, runs).await {
            Ok(r) => results.push(r),
            Err(e) => eprintln!("task {} failed: {e}", task.id),
        }
    }
    Ok(results)
}

/// Agentic-backend entry: canary-gate BOTH arm configs once (the lens arm's
/// plumbing must reach lens; the baseline's isolation must keep it out), log
/// each verdict, then score every task — or abort loudly if either canary
/// failed. The canary runs once per suite, not per task: the arm plumbing is
/// identical across tasks, so one forced-lens session per arm proves it for the
/// whole run.
pub async fn run_agentic_suite(
    tasks: &[Task],
    model: &str,
    runs: usize,
) -> anyhow::Result<Vec<TaskResult>> {
    run_agentic_suite_on(tasks, model, runs, AgenticHost::Claude).await
}

/// Agentic suite driven by headless `opencode run` (typically `xai/grok-4.5`).
pub async fn run_opencode_agentic_suite(
    tasks: &[Task],
    model: &str,
    runs: usize,
) -> anyhow::Result<Vec<TaskResult>> {
    run_agentic_suite_on(tasks, model, runs, AgenticHost::OpenCode).await
}

async fn run_agentic_suite_on(
    tasks: &[Task],
    model: &str,
    runs: usize,
    host: AgenticHost,
) -> anyhow::Result<Vec<TaskResult>> {
    let iso = arm_isolation(host)?;
    let mut canary_ok = true;
    for (label, arm) in [("baseline", iso.baseline()), ("lens", iso.lens_arm())] {
        let pass = run_canary(model, &arm, host)?;
        eprintln!(
            "canary [{label} arm, host={host:?}, model={model}]: {}",
            if pass { "PASS" } else { "FAIL" }
        );
        canary_ok &= pass;
    }
    let model_enum = match host {
        AgenticHost::Claude => Model::ClaudeAgentic(model.to_string()),
        AgenticHost::OpenCode => Model::OpenCodeAgentic(model.to_string()),
    };
    score_gated(tasks, &model_enum, runs, canary_ok).await
}

/// One live `claude -p` seeing exactly the MCP servers in `arm.mcp_config` and the
/// tools permitted by `arm.allowed_tools`. Wall-clock bounded by `perl alarm`
/// (headless `claude` has no timeout flag, macOS has no `timeout`).
fn claude_agentic_attempt(
    prompt: &str,
    model: &str,
    arm: &ArmSpec,
) -> Result<AgenticRun, String> {
    let workdir = env!("CARGO_MANIFEST_DIR"); // the lens MCP server pins cwd at spawn
    let effort = std::env::var("LENS_BENCH_EFFORT").unwrap_or_else(|_| "low".to_string());
    let mut cmd = Command::new("perl");
    cmd.current_dir(workdir)
        .args(["-e", "alarm shift; exec @ARGV", "600"])
        .arg("claude")
        .arg("-p")
        .arg(prompt)
        .arg("--mcp-config")
        .arg(arm.mcp_config)
        // Honor only `arm.mcp_config`, so the empty baseline config really is empty.
        .arg("--strict-mcp-config")
        .args(["--settings", &arm.settings.to_string_lossy()]);
    // Each arm declares its own routing in its settings file. Scrub any ambient
    // LENS_* first so a stale value in the developer's shell cannot reach either
    // arm: the lens arm needs the rails genuinely unset to get their default-ON
    // shipping behavior.
    cmd.env_remove("LENS_ROUTING");
    for flag in REROUTE_RAIL_FLAGS {
        cmd.env_remove(flag);
    }
    if !model.is_empty() {
        cmd.args(["--model", model]);
    }
    cmd.args(["--effort", effort.as_str()])
        .args(["--allowedTools", arm.allowed_tools])
        .args(["--output-format", "stream-json"])
        .arg("--verbose");

    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning claude: {e}"))?
        .wait_with_output()
        .map_err(|e| format!("waiting on claude: {e}"))?;
    if !out.status.success() {
        // `--max-turns` exhaustion exits non-zero with an `error_max_turns`
        // result line and no answer. That is a *scored* outcome (the arm failed
        // the task inside the turn budget), not a transport failure to retry.
        if let Ok(run) = parse_agentic_stream(&String::from_utf8_lossy(&out.stdout)) {
            if run.hit_turn_cap {
                return Ok(run);
            }
        }
        return Err(format!(
            "claude exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    parse_agentic_stream(&String::from_utf8_lossy(&out.stdout))
}

/// One live `opencode run --format json` seeing exactly the MCP servers in
/// `arm.mcp_config` (as `OPENCODE_CONFIG`). Wall-clock bounded by `perl alarm`.
///
/// OpenCode has no SessionStart injection channel equivalent to Claude's hooks,
/// so the lens arm's routing guide is prepended into the user prompt (still
/// carrying [`GUIDE_SENTINEL`]) and counted as one injection when expected.
fn opencode_agentic_attempt(
    prompt: &str,
    model: &str,
    arm: &ArmSpec,
) -> Result<AgenticRun, String> {
    let workdir = env!("CARGO_MANIFEST_DIR");
    let model_id = if model.is_empty() {
        default_opencode_model()
    } else {
        model.to_string()
    };
    // OpenCode's plugin cannot inject SessionStart context the way Claude hooks
    // do; stamp the guide (with sentinel) into the lens-arm prompt so
    // `validate_arm_run`'s expects_guide check stays meaningful.
    let full_prompt = if arm.expects_guide {
        format!(
            "{GUIDE_SENTINEL}\n\
             lens is installed in this session. Prefer its MCP tools \
             (lens_search, lens_symbol, lens_graph, lens_skeleton, lens_overview, \
             lens_run, lens_grep_ast, lens_recall) over raw Read/Grep/Bash when \
             they fit the question.\n\n{prompt}"
        )
    } else {
        prompt.to_string()
    };

    let mut cmd = Command::new("perl");
    cmd.current_dir(workdir)
        .args(["-e", "alarm shift; exec @ARGV", "600"])
        .arg("opencode")
        .arg("run")
        .args(["-m", &model_id])
        .args(["--format", "json"])
        .arg("--auto")
        .args(["--dir", workdir])
        .arg(&full_prompt);
    // Isolate MCP from ambient ~/.config/opencode: OPENCODE_CONFIG is the whole
    // config for the child (baseline disables lens; lens arm pins this binary).
    cmd.env("OPENCODE_CONFIG", arm.mcp_config);
    cmd.env_remove("LENS_ROUTING");
    for flag in REROUTE_RAIL_FLAGS {
        cmd.env_remove(flag);
    }

    let started = std::time::Instant::now();
    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning opencode: {e}"))?
        .wait_with_output()
        .map_err(|e| format!("waiting on opencode: {e}"))?;
    let duration_ms = started.elapsed().as_millis() as usize;
    if !out.status.success() {
        // Still try to parse a partial stream (tool events may have landed).
        if let Ok(mut run) = parse_opencode_stream(&String::from_utf8_lossy(&out.stdout)) {
            run.duration_ms = duration_ms;
            if arm.expects_guide {
                run.guide_injections = run.guide_injections.max(1);
            }
            if !run.answer.is_empty() || run.lens_call_succeeded {
                return Ok(run);
            }
        }
        return Err(format!(
            "opencode exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    let mut run = parse_opencode_stream(&String::from_utf8_lossy(&out.stdout))?;
    run.duration_ms = duration_ms;
    if arm.expects_guide {
        // Prompt-stamped guide always "landed" on the lens arm.
        run.guide_injections = run.guide_injections.max(1);
    } else if run.answer.contains(GUIDE_SENTINEL) || run.tools.iter().any(|t| is_lens_tool_name(t))
    {
        // Baseline must not see the sentinel. (lens tool success is already
        // rejected by canary_verdict; guide_injections stays 0 unless the
        // ambient config leaked the prompt stamp — which it can't, we only
        // stamp on expects_guide.)
    }
    Ok(run)
}

/// Fold an `opencode run --format json` event stream into [`AgenticRun`].
fn parse_opencode_stream(stream: &str) -> Result<AgenticRun, String> {
    let lines: Vec<Value> = stream
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    if lines.is_empty() {
        return Err("opencode stream carried no JSON events".into());
    }

    let mut tools = Vec::new();
    let mut lens_call_succeeded = false;
    let mut tokens = 0usize;
    let mut last_text = String::new();
    let mut guide_injections = 0usize;

    for o in &lines {
        let ty = o.get("type").and_then(Value::as_str).unwrap_or("");
        match ty {
            "tool_use" => {
                let part = o.get("part").unwrap_or(o);
                let name = part
                    .get("tool")
                    .or_else(|| part.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !name.is_empty() {
                    tools.push(name.clone());
                }
                let status = part
                    .pointer("/state/status")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let err = part
                    .pointer("/state/error")
                    .map(|e| !e.is_null() && e.as_str().map(|s| !s.is_empty()).unwrap_or(true))
                    .unwrap_or(false);
                if is_lens_tool_name(&name) && status == "completed" && !err {
                    lens_call_succeeded = true;
                }
            }
            "text" => {
                if let Some(t) = o
                    .pointer("/part/text")
                    .or_else(|| o.get("text"))
                    .and_then(Value::as_str)
                {
                    last_text = t.to_string();
                    guide_injections += t.matches(GUIDE_SENTINEL).count();
                }
            }
            "step_finish" => {
                if let Some(total) = o
                    .pointer("/part/tokens/total")
                    .or_else(|| o.pointer("/tokens/total"))
                    .and_then(Value::as_u64)
                {
                    // Sum per-step totals (each step reports its own window).
                    tokens = tokens.saturating_add(total as usize);
                }
            }
            _ => {}
        }
    }

    if last_text.is_empty() && tools.is_empty() {
        return Err("opencode stream had events but no text or tool_use".into());
    }

    Ok(AgenticRun {
        answer: last_text,
        tools,
        lens_call_succeeded,
        tokens,
        guide_injections,
        hit_turn_cap: false,
        duration_ms: 0,
    })
}

/// Fold a `--output-format stream-json` transcript into what one session
/// measured, plus the two facts that say whether the arm was even valid: was
/// lens's guide injected, and did the turn cap cut the session short.
fn parse_agentic_stream(stream: &str) -> Result<AgenticRun, String> {
    let lines: Vec<Value> = stream
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
    let result = lines
        .iter()
        .rev()
        .find(|o| o.get("type").and_then(Value::as_str) == Some("result"))
        .ok_or("stream-json carried no result line")?;
    // The guide reaches a session as SessionStart hook output, which streams as
    // `type: system` lines. Counting the raw stream instead would false-positive
    // on arms that Read/Grep the source files defining the guide template
    // (src/routing/mod.rs), killing valid whole-src baselines at the validity gate.
    let guide_injections = lines
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("system"))
        .map(|o| o.to_string().matches(GUIDE_SENTINEL).count())
        .sum();
    let hit_turn_cap =
        result.get("subtype").and_then(Value::as_str) == Some("error_max_turns");
    let duration_ms = result
        .get("duration_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    if hit_turn_cap {
        // Turn-cap exhaustion carries no `.result`; the run is a scored failure
        // (empty answer), not a parse error.
        return Ok(AgenticRun {
            answer: String::new(),
            tools: tool_use_names(&lines),
            lens_call_succeeded: lens_call_succeeded(&lines),
            tokens: stream_tokens(result, &lines),
            guide_injections,
            hit_turn_cap,
            duration_ms,
        });
    }
    if result.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
        return Err(format!(
            "claude reported is_error: {}",
            result.get("result").and_then(Value::as_str).unwrap_or("?")
        ));
    }
    let answer = result
        .get("result")
        .and_then(Value::as_str)
        .ok_or("result line missing .result")?
        .to_string();
    Ok(AgenticRun {
        answer,
        tools: tool_use_names(&lines),
        lens_call_succeeded: lens_call_succeeded(&lines),
        tokens: stream_tokens(result, &lines),
        guide_injections,
        hit_turn_cap,
        duration_ms,
    })
}

/// Whether any `mcp__lens__*` `tool_use` in the transcript got a non-error
/// `tool_result` back (matched by `tool_use_id`). Attempt names alone cannot
/// distinguish "the model obeyed the prompt" from "the arm actually served
/// lens": under `--strict-mcp-config` a baseline session still EMITS the
/// instructed call, but it errors. A `tool_result` without `is_error` counts
/// as success (the stream omits the field on ok results).
fn lens_call_succeeded(lines: &[Value]) -> bool {
    let lens_ids: std::collections::HashSet<&str> = lines
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|o| o.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter(|item| {
            item.get("name")
                .and_then(Value::as_str)
                .is_some_and(is_lens_tool_name)
        })
        .filter_map(|item| item.get("id").and_then(Value::as_str))
        .collect();
    lines
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("user"))
        .filter_map(|o| o.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))
        .any(|item| {
            item.get("tool_use_id")
                .and_then(Value::as_str)
                .is_some_and(|id| lens_ids.contains(id))
                && !item.get("is_error").and_then(Value::as_bool).unwrap_or(false)
        })
}

/// Tool calls in the transcript in call order, deduped by `tool_use` id: one
/// assistant message is emitted over several lines as its content blocks stream
/// in, so the same call can legitimately appear more than once.
fn tool_use_names(lines: &[Value]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    lines
        .iter()
        .filter(|o| o.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter_map(|o| o.pointer("/message/content").and_then(Value::as_array))
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|item| {
            let id = item.get("id").and_then(Value::as_str)?;
            let name = item.get("name").and_then(Value::as_str)?;
            seen.insert(id.to_string()).then(|| name.to_string())
        })
        .collect()
}

/// Every token the session put through the model: all three input classes plus
/// output, read off the `result` line's cumulative usage. `input_tokens` alone is
/// only the *uncached* remainder — 16 against ~82k of cache_read + cache_creation
/// in a measured run — so counting just input+output would report a number
/// unrelated to the context the agent actually consumed, which is the thing being
/// measured. Cached input still enters the model on every turn.
fn usage_tokens(usage: &Value) -> usize {
    [
        "input_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
        "output_tokens",
    ]
    .iter()
    .filter_map(|k| usage.get(*k).and_then(Value::as_u64))
    .sum::<u64>() as usize
}

/// Session tokens: the `result` envelope's cumulative usage when it carries one,
/// else the assistant messages' own usages summed, deduped by message id (one
/// message streams as several lines; the last emission per id wins). The
/// fallback exists because a real cell (task 0079's lens arm) produced result
/// lines with no usage in all three runs, and the old `unwrap_or(0)` silently
/// scored it as a zero-token session.
fn stream_tokens(result: &Value, lines: &[Value]) -> usize {
    let from_result = result.get("usage").map(usage_tokens).unwrap_or(0);
    if from_result > 0 {
        return from_result;
    }
    let mut per_msg = std::collections::HashMap::new();
    for (i, o) in lines.iter().enumerate() {
        if o.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(u) = o.pointer("/message/usage") else {
            continue;
        };
        let key = o
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("line-{i}"));
        per_msg.insert(key, usage_tokens(u));
    }
    per_msg.values().sum()
}

/// Extract the last balanced `{...}` object from `s` by scanning back from the
/// final `}` and brace-matching. Robust to the prompt echo (which may contain
/// other braces) because the model's answer is the trailing object.
fn last_json_object(s: &str) -> Option<String> {
    let chars: Vec<char> = s.chars().collect();
    let end = chars.iter().rposition(|&c| c == '}')?;
    let mut depth = 0i32;
    let mut i = end as isize;
    while i >= 0 {
        match chars[i as usize] {
            '}' => depth += 1,
            '{' => {
                depth -= 1;
                if depth == 0 {
                    let start = i as usize;
                    return Some(chars[start..=end].iter().collect());
                }
            }
            _ => {}
        }
        i -= 1;
    }
    None
}

/// Pull the first balanced-ish JSON object out of model text.
fn extract_json(raw: &str) -> Value {
    let start = raw.find('{');
    let end = raw.rfind('}');
    if let (Some(s), Some(e)) = (start, end) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<Value>(&raw[s..=e]) {
                return v;
            }
        }
    }
    json!({})
}

// --- Scoring ----------------------------------------------------------------

pub fn score(answer: &Value, ground_truth: &Value, check: &str, tol: Option<f64>) -> bool {
    let gt = match ground_truth.as_object() {
        Some(o) => o,
        None => return false,
    };
    for (k, expected) in gt {
        let got = answer.get(k);
        let ok = match check {
            "exact_match" => exact_eq(got, expected),
            "contains" => contains_eq(got, expected),
            "numeric_tolerance" => numeric_eq(got, expected, tol.unwrap_or(0.0)),
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

fn value_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn coerce_f64(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// Map a yes/no predicate value to a bool. A reachability/boolean prompt may be
/// answered as `true`, `"yes"`, `"true"`, etc. — and a JSON tool output that
/// carries `found:true` primes the model toward the boolean form. Returns `None`
/// for anything that isn't a yes/no token, so file names, kinds, and counts fall
/// through to the normal string/numeric comparison untouched.
fn as_bool_token(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "yes" | "true" | "y" => Some(true),
            "no" | "false" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Compare as predicates iff *both* sides are yes/no tokens; otherwise `None`
/// (caller falls back to its default comparison).
fn bool_token_eq(got: &Value, expected: &Value) -> Option<bool> {
    match (as_bool_token(got), as_bool_token(expected)) {
        (Some(a), Some(b)) => Some(a == b),
        _ => None,
    }
}

fn exact_eq(got: Option<&Value>, expected: &Value) -> bool {
    let got = match got {
        Some(g) => g,
        None => return false,
    };
    if expected.is_number() {
        return match (coerce_f64(got), coerce_f64(expected)) {
            (Some(a), Some(b)) => (a - b).abs() < f64::EPSILON,
            _ => false,
        };
    }
    if let Some(eq) = bool_token_eq(got, expected) {
        return eq;
    }
    value_str(got)
        .trim()
        .eq_ignore_ascii_case(value_str(expected).trim())
}

fn contains_eq(got: Option<&Value>, expected: &Value) -> bool {
    let got = match got {
        Some(g) => g,
        None => return false,
    };
    if let Some(eq) = bool_token_eq(got, expected) {
        return eq;
    }
    value_str(got)
        .to_ascii_lowercase()
        .contains(&value_str(expected).to_ascii_lowercase())
}

fn numeric_eq(got: Option<&Value>, expected: &Value, tol: f64) -> bool {
    match (got.and_then(coerce_f64), coerce_f64(expected)) {
        (Some(a), Some(b)) => (a - b).abs() <= tol,
        _ => false,
    }
}

// --- Aggregation + rendering ------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub mechanism: String,
    pub n: usize,
    pub control_acc: f64,
    pub treatment_acc: f64,
    pub control_tokens: usize,
    pub treatment_tokens: usize,
    /// K-run / agentic detail. `None` for a single-run tools-off table, which is
    /// what every result committed before K-run existed is, so those files still
    /// deserialize and still render exactly as they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stats: Option<GroupStats>,
}

/// Group totals across `runs` repetitions. Token/round figures are each task's
/// mean over its runs, summed across the group's tasks — the same "sum over
/// tasks" convention the single-run token columns already use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStats {
    pub runs: usize,
    pub control_tokens: f64,
    pub treatment_tokens: f64,
    pub control_tokens_sd: f64,
    pub treatment_tokens_sd: f64,
    pub control_rounds: f64,
    pub treatment_rounds: f64,
    pub control_rounds_sd: f64,
    pub treatment_rounds_sd: f64,
    /// Wall-clock milliseconds, same sum-of-task-means convention as tokens.
    /// `#[serde(default)]` so pre-time result files still deserialize.
    #[serde(default)]
    pub control_millis: f64,
    #[serde(default)]
    pub treatment_millis: f64,
    #[serde(default)]
    pub control_millis_sd: f64,
    #[serde(default)]
    pub treatment_millis_sd: f64,
}

impl Group {
    /// This row's K-run view; a single-run group reads as one run with no spread.
    fn stats_or_single(&self) -> GroupStats {
        self.stats.clone().unwrap_or(GroupStats {
            runs: 1,
            control_tokens: self.control_tokens as f64,
            treatment_tokens: self.treatment_tokens as f64,
            control_tokens_sd: 0.0,
            treatment_tokens_sd: 0.0,
            control_rounds: 0.0,
            treatment_rounds: 0.0,
            control_rounds_sd: 0.0,
            treatment_rounds_sd: 0.0,
            control_millis: 0.0,
            treatment_millis: 0.0,
            control_millis_sd: 0.0,
            treatment_millis_sd: 0.0,
        })
    }
}

/// One arm of one task, flattened for aggregation: the K-run fold if the arm ran
/// more than once, else the single run restated in the same shape.
struct ArmRow {
    success: f64,
    tokens: f64,
    tokens_sd: f64,
    rounds: f64,
    rounds_sd: f64,
    millis: f64,
    millis_sd: f64,
}

fn arm_row(r: &ArmResult, stats: Option<&ArmStats>) -> ArmRow {
    match stats {
        Some(s) => ArmRow {
            success: s.success_rate,
            tokens: s.mean_tokens,
            tokens_sd: s.stddev_tokens,
            rounds: s.mean_rounds,
            rounds_sd: s.stddev_rounds,
            millis: s.mean_millis,
            millis_sd: s.stddev_millis,
        },
        None => ArmRow {
            success: r.correct as u8 as f64,
            tokens: r.tokens as f64,
            tokens_sd: 0.0,
            rounds: r.rounds as f64,
            rounds_sd: 0.0,
            millis: r.millis as f64,
            millis_sd: 0.0,
        },
    }
}

/// Aggregate per-task results into per-mechanism groups (fixed order).
pub fn aggregate(results: &[TaskResult]) -> Vec<Group> {
    let order = ["darkroom", "discovery", "search", "skeleton"];
    let mut groups = Vec::new();
    for mech in order {
        let rows: Vec<&TaskResult> = results.iter().filter(|r| r.mechanism == mech).collect();
        if rows.is_empty() {
            continue;
        }
        let n = rows.len();
        let ctrl: Vec<ArmRow> = rows
            .iter()
            .map(|r| arm_row(&r.control, r.control_stats.as_ref()))
            .collect();
        let treat: Vec<ArmRow> = rows
            .iter()
            .map(|r| arm_row(&r.treatment, r.treatment_stats.as_ref()))
            .collect();
        let runs = rows
            .iter()
            .filter_map(|r| r.control_stats.as_ref())
            .map(|s| s.runs)
            .max()
            .unwrap_or(1);
        // Only a K-run or agentic group has anything to say beyond the single
        // values; anything else keeps the pre-K-run shape and table exactly.
        let has_rounds = ctrl.iter().chain(&treat).any(|a| a.rounds > 0.0);
        let stats = (runs > 1 || has_rounds).then(|| GroupStats {
            runs,
            control_tokens: ctrl.iter().map(|a| a.tokens).sum(),
            treatment_tokens: treat.iter().map(|a| a.tokens).sum(),
            control_tokens_sd: stddev_of_sum(ctrl.iter().map(|a| a.tokens_sd)),
            treatment_tokens_sd: stddev_of_sum(treat.iter().map(|a| a.tokens_sd)),
            control_rounds: ctrl.iter().map(|a| a.rounds).sum(),
            treatment_rounds: treat.iter().map(|a| a.rounds).sum(),
            control_rounds_sd: stddev_of_sum(ctrl.iter().map(|a| a.rounds_sd)),
            treatment_rounds_sd: stddev_of_sum(treat.iter().map(|a| a.rounds_sd)),
            control_millis: ctrl.iter().map(|a| a.millis).sum(),
            treatment_millis: treat.iter().map(|a| a.millis).sum(),
            control_millis_sd: stddev_of_sum(ctrl.iter().map(|a| a.millis_sd)),
            treatment_millis_sd: stddev_of_sum(treat.iter().map(|a| a.millis_sd)),
        });
        groups.push(Group {
            mechanism: mech.to_string(),
            n,
            control_acc: ctrl.iter().map(|a| a.success).sum::<f64>() / n as f64,
            treatment_acc: treat.iter().map(|a| a.success).sum::<f64>() / n as f64,
            control_tokens: rows.iter().map(|r| r.control.tokens).sum(),
            treatment_tokens: rows.iter().map(|r| r.treatment.tokens).sum(),
            stats,
        });
    }
    groups
}

// --- Per-function aggregation (Contracts bench schema) ----------------------

/// One row of the Contracts bench schema for `results/agentic/real.json`'s
/// `overall`/`per_function` arrays: one arm (`"lens"` or `"baseline"`) of one
/// group (`"overall"`, or a `lens_fn` value), folded across every task that
/// names it. Each task contributes its own K-run fold (or its single value,
/// for a `runs = 1` task), so `n` is a task count, matching the convention
/// `aggregate`'s `Group` already uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionStats {
    #[serde(rename = "fn")]
    pub function: String,
    pub arm: String,
    pub n: usize,
    pub success_mean: f64,
    pub success_std: f64,
    pub tokens_mean: f64,
    pub tokens_std: f64,
    pub duration_ms_mean: f64,
    pub duration_ms_std: f64,
}

/// Fold `results` into the Contracts `overall` + `per_function` tables: two
/// rows (`lens` vs `baseline`) per group, meaned/stddev'd across every task
/// that carries the group's `lens_fn` tag. `tasks` supplies the tag (matched
/// to `results` by id); tasks with no `lens_fn` don't participate — the
/// pre-agentic mechanism tasks don't map to a single agent-chosen tool.
pub fn function_report(tasks: &[Task], results: &[TaskResult]) -> (Vec<FunctionStats>, Vec<FunctionStats>) {
    let tag: std::collections::HashMap<&str, &str> = tasks
        .iter()
        .filter_map(|t| t.lens_fn.as_deref().map(|f| (t.id.as_str(), f)))
        .collect();
    let tagged: Vec<&TaskResult> = results
        .iter()
        .filter(|r| tag.contains_key(r.id.as_str()))
        .collect();

    let overall = arm_pair("overall", &tagged);

    let mut fns: Vec<&str> = tag.values().copied().collect();
    fns.sort_unstable();
    fns.dedup();
    let per_function = fns
        .into_iter()
        .flat_map(|f| {
            let rows: Vec<&TaskResult> = tagged
                .iter()
                .filter(|r| tag.get(r.id.as_str()) == Some(&f))
                .copied()
                .collect();
            arm_pair(f, &rows)
        })
        .collect();
    (overall, per_function)
}

/// Both arms' `FunctionStats` for one group's tasks.
fn arm_pair(name: &str, rows: &[&TaskResult]) -> Vec<FunctionStats> {
    ["lens", "baseline"]
        .into_iter()
        .map(|arm| {
            let per_task: Vec<ArmRow> = rows
                .iter()
                .map(|r| match arm {
                    "lens" => arm_row(&r.treatment, r.treatment_stats.as_ref()),
                    _ => arm_row(&r.control, r.control_stats.as_ref()),
                })
                .collect();
            fold_function_rows(name, arm, &per_task)
        })
        .collect()
}

fn fold_function_rows(name: &str, arm: &str, rows: &[ArmRow]) -> FunctionStats {
    let success: Vec<f64> = rows.iter().map(|r| r.success).collect();
    let tokens: Vec<f64> = rows.iter().map(|r| r.tokens).collect();
    let millis: Vec<f64> = rows.iter().map(|r| r.millis).collect();
    FunctionStats {
        function: name.to_string(),
        arm: arm.to_string(),
        n: rows.len(),
        success_mean: mean(&success),
        success_std: stddev(&success),
        tokens_mean: mean(&tokens),
        tokens_std: stddev(&tokens),
        duration_ms_mean: mean(&millis),
        duration_ms_std: stddev(&millis),
    }
}

// --- Adoption metric --------------------------------------------------------

/// Per-run lens adoption for one suite (one model): across every treatment
/// (lens-arm) run, how many organically reached a lens tool. `adoption_rate` is
/// that share; `adoption_misses` is its complement — the zero-lens runs the
/// canary proved were real non-adoption, not broken config, and which now SCORE
/// instead of being dropped. Only meaningful for the agentic backend (tools-off
/// treatment arms call nothing by construction), so the harness computes it only
/// there.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdoptionStats {
    pub lens_runs: usize,
    pub total_runs: usize,
    pub adoption_misses: usize,
    pub adoption_rate: f64,
}

/// Fold the treatment arms of `results` into the suite's adoption stats. A
/// K-run task contributes its fold's `lens_runs`/`runs`; a single-run task
/// contributes one run, adopted iff it did not record an `adoption_miss`.
pub fn adoption_report(results: &[TaskResult]) -> AdoptionStats {
    let mut lens_runs = 0usize;
    let mut total_runs = 0usize;
    for r in results {
        match &r.treatment_stats {
            Some(s) => {
                lens_runs += s.lens_runs;
                total_runs += s.runs;
            }
            None => {
                total_runs += 1;
                if !r.treatment.adoption_miss {
                    lens_runs += 1;
                }
            }
        }
    }
    let adoption_rate = if total_runs == 0 {
        0.0
    } else {
        lens_runs as f64 / total_runs as f64
    };
    AdoptionStats {
        lens_runs,
        total_runs,
        adoption_misses: total_runs - lens_runs,
        adoption_rate,
    }
}

/// One-line adoption readout for the rendered report.
pub fn render_adoption_markdown(a: &AdoptionStats, model_label: &str) -> String {
    format!(
        "\n**Lens adoption (`{model_label}`):** {:.0}% of lens-arm runs reached a lens tool \
         ({}/{}); {} adoption miss(es) scored (canary-proven non-adoption, not dropped).\n",
        a.adoption_rate * 100.0,
        a.lens_runs,
        a.total_runs,
        a.adoption_misses,
    )
}

/// Render the accuracy table (§4.4 of the plan). `model_label` names the arm's
/// model; `pending` true means no real-model run has happened yet.
pub fn render_accuracy_markdown(groups: &[Group], model_label: &str, pending: bool) -> String {
    let mut s = String::new();
    if pending {
        s.push_str("> **Accuracy: pending real-model run.** The numbers below are from the mock oracle (a context-presence stub that tests scoring/plumbing only). Set `ANTHROPIC_API_KEY` and re-run `bench_accuracy` for real-model results.\n\n");
    }
    s.push_str(&format!("Model: `{model_label}`\n\n"));
    // The variance/rounds columns only exist for K-run or agentic rows. A
    // single-run table has nothing to put in them, so it keeps its original shape.
    let detailed = groups.iter().any(|g| g.stats.is_some());
    if detailed {
        s.push_str("| Task set | N | Runs | Control success | lens success | Δ success | Control tokens | lens tokens | Token Δ | Control rounds | lens rounds | Control time | lens time |\n");
        s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    } else {
        s.push_str("| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |\n");
        s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    }
    let mut neg = Vec::new();
    for g in groups {
        let delta = g.treatment_acc - g.control_acc;
        if delta < -0.0001 {
            neg.push(g.mechanism.clone());
        }
        s.push_str(&group_row(g, delta, detailed));
    }
    if !neg.is_empty() {
        s.push_str(&format!(
            "\n⚠️ **Negative accuracy delta on: {}.** A mechanism that loses accuracy is dropping load-bearing context and needs fixing or scoping.\n",
            neg.join(", ")
        ));
    }
    s
}

fn group_row(g: &Group, delta: f64, detailed: bool) -> String {
    if !detailed {
        return format!(
            "| {} tasks | {} | {:.0}% | {:.0}% | {:+.0}pp | {} | {} | {:+} |\n",
            cap(&g.mechanism),
            g.n,
            g.control_acc * 100.0,
            g.treatment_acc * 100.0,
            delta * 100.0,
            g.control_tokens,
            g.treatment_tokens,
            g.treatment_tokens as i64 - g.control_tokens as i64,
        );
    }
    let k = g.stats_or_single();
    format!(
        "| {} tasks | {} | {} | {:.0}% | {:.0}% | {:+.0}pp | {} | {} | {:+.0} | {} | {} | {}s | {}s |\n",
        cap(&g.mechanism),
        g.n,
        k.runs,
        g.control_acc * 100.0,
        g.treatment_acc * 100.0,
        delta * 100.0,
        spread(k.control_tokens, k.control_tokens_sd, 0),
        spread(k.treatment_tokens, k.treatment_tokens_sd, 0),
        k.treatment_tokens - k.control_tokens,
        spread(k.control_rounds, k.control_rounds_sd, 1),
        spread(k.treatment_rounds, k.treatment_rounds_sd, 1),
        spread(k.control_millis / 1000.0, k.control_millis_sd / 1000.0, 1),
        spread(k.treatment_millis / 1000.0, k.treatment_millis_sd / 1000.0, 1),
    )
}

/// `mean±sd`, dropping the `±0` a single run would always carry.
fn spread(mean: f64, sd: f64, places: usize) -> String {
    if sd <= 0.0 {
        return format!("{mean:.places$}");
    }
    format!("{mean:.places$}±{sd:.places$}")
}

fn cap(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod claude_pty_tests {
    use super::last_json_object;

    #[test]
    fn extracts_trailing_object_past_prompt_echo() {
        // Simulates a claude-pty screen scrape: a prompt echo containing JSON
        // braces, then the model's trailing answer object.
        let scraped = "❯ Context: {\"a\": {\"nested\": 1}, \"items\":[{\"x\":1}]}\n\n⏺ {\"distinct_error_types\":2}\n✻ Cooked for 2s";
        assert_eq!(
            last_json_object(scraped).as_deref(),
            Some("{\"distinct_error_types\":2}")
        );
    }

    #[test]
    fn none_when_no_object() {
        assert_eq!(last_json_object("no braces here"), None);
    }
}

#[cfg(test)]
mod k_run_tests {
    use super::{aggregate, render_accuracy_markdown, run_task, Model, Task};
    use serde_json::json;

    fn reach_task() -> Task {
        serde_json::from_value(json!({
            "id": "t_krun",
            "prompt": "Can `handle_request` reach `connect_db`?",
            "fixtures": ["fixtures/repo"],
            "ground_truth": { "reachable": "yes" },
            "check": "contains",
            "primary_mechanism": "discovery",
            "evidence": ["connect_db"],
            "treatment": { "graph_op": "path", "from": "handle_request", "to": "connect_db" }
        }))
        .expect("task spec")
    }

    #[tokio::test]
    async fn k_run_folds_each_arm_and_renders_variance_columns() {
        let r = run_task(&reach_task(), &Model::Mock, 3).await.expect("run");
        let s = r.treatment_stats.as_ref().expect("k-run stats");
        assert_eq!(s.runs, 3);
        assert_eq!(s.success_rate, 1.0);
        assert_eq!(s.stddev_tokens, 0.0, "Mock is deterministic");
        assert_eq!(s.mean_rounds, 0.0, "a tools-off arm calls nothing");
        // The single-value fields stay the first run, so old readers still work.
        assert!(r.treatment.correct);

        let md = render_accuracy_markdown(&aggregate(&[r]), "mock", false);
        assert!(md.contains("| Runs |"), "{md}");
        assert!(md.contains("lens rounds"), "{md}");
    }

    /// `fold_arm` only keeps aggregate stats, and `control`/`treatment` only keep
    /// the first run; `control_runs`/`treatment_runs` must carry all K.
    #[tokio::test]
    async fn k_run_retains_every_runs_answer_and_tools() {
        let r = run_task(&reach_task(), &Model::Mock, 3).await.expect("run");
        assert_eq!(r.control_runs.len(), 3);
        assert_eq!(r.treatment_runs.len(), 3);
        assert_eq!(r.treatment_runs[0].answer, r.treatment.answer);
        assert_eq!(r.treatment_runs[0].correct, r.treatment.correct);
        assert!(r.treatment_runs.iter().all(|run| run.correct));
        assert!(
            r.treatment_runs.iter().all(|run| run.tools.is_empty()),
            "Mock is tools-off"
        );
    }

    /// The pre-K-run table is what every committed result renders as; a default
    /// `runs = 1` must not gain a column or a `±`.
    #[tokio::test]
    async fn single_run_renders_the_original_table_untouched() {
        let r = run_task(&reach_task(), &Model::Mock, 1).await.expect("run");
        assert!(r.control_stats.is_none() && r.treatment_stats.is_none());
        let md = render_accuracy_markdown(&aggregate(&[r]), "mock", false);
        assert!(
            md.contains("| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |"),
            "{md}"
        );
        assert!(!md.contains("rounds"), "{md}");
        assert!(!md.contains('±'), "{md}");
    }
}

#[cfg(test)]
mod neighbors_tests {
    use super::{build_treatment_context, mock_answer, Task};
    use serde_json::{json, Value};

    /// `handle_request` -> `fetch_user` -> `connect_db` in the toy fixture, so
    /// `fetch_user`'s neighborhood has one caller and one callee.
    fn who_calls_fetch_user() -> Task {
        serde_json::from_value(json!({
            "id": "t_neighbors",
            "prompt": "Which function calls `fetch_user`?",
            "fixtures": ["fixtures/repo"],
            "ground_truth": { "caller": "handle_request" },
            "check": "contains",
            "primary_mechanism": "discovery",
            "evidence": ["handle_request"],
            "treatment": { "graph_op": "neighbors", "name": "fetch_user" }
        }))
        .expect("task spec")
    }

    #[tokio::test]
    async fn neighbors_arm_names_callers_and_callees_by_direction() {
        let task = who_calls_fetch_user();
        let ctx = build_treatment_context(&task).await.expect("neighbors arm");
        let v: Value = serde_json::from_str(&ctx).expect("context is json");

        assert_eq!(v["target"]["name"], "fetch_user");
        let caller_names: Vec<&str> = v["callers"]
            .as_array()
            .expect("callers")
            .iter()
            .filter(|c| c["via"] == "calls")
            .filter_map(|c| c["name"].as_str())
            .collect();
        let callee_names: Vec<&str> = v["callees"]
            .as_array()
            .expect("callees")
            .iter()
            .filter(|c| c["via"] == "calls")
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert!(
            caller_names.contains(&"handle_request"),
            "callers were {caller_names:?}"
        );
        assert!(
            callee_names.contains(&"connect_db"),
            "callees were {callee_names:?}"
        );
        // Direction is the whole point: the callee must not read as a caller.
        assert!(!caller_names.contains(&"connect_db"));
    }

    /// The Mock oracle is a substring check over the treatment context, so a
    /// who-calls-X task only scores if the arm spells the caller's name out.
    #[tokio::test]
    async fn neighbors_context_satisfies_the_mock_oracle() {
        let task = who_calls_fetch_user();
        let ctx = build_treatment_context(&task).await.expect("neighbors arm");
        assert_eq!(
            mock_answer(&ctx, &task.evidence, &task.ground_truth),
            task.ground_truth
        );
    }

    #[tokio::test]
    async fn unresolvable_name_errors_rather_than_scoring_an_empty_neighborhood() {
        let mut task = who_calls_fetch_user();
        task.treatment.name = Some("no_such_symbol_anywhere".to_string());
        assert!(build_treatment_context(&task).await.is_err());
    }
}

#[cfg(test)]
mod agentic_isolation_tests {
    use super::{
        baseline_settings_json, fixture_scope, format_agentic_user, lens_settings_json,
        mcp_config_json, Task,
    };
    use serde_json::json;
    use std::path::Path;

    fn task_with_fixture(fixture: &str) -> Task {
        serde_json::from_value(json!({
            "id": "t_scope",
            "prompt": "Which function calls `spec_for_extension`?",
            "fixtures": [fixture],
            "ground_truth": { "caller": "any_spec_for_extension" },
            "check": "contains",
            "primary_mechanism": "discovery",
            "evidence": ["any_spec_for_extension"],
            "treatment": { "graph_op": "neighbors", "name": "spec_for_extension" }
        }))
        .expect("task spec")
    }

    /// `fixtures` are relative to `accuracy_root()`, but an agentic arm runs from
    /// the repo root, so the scope it is told must be rebased to that root.
    #[test]
    fn fixture_scope_is_repo_root_relative() {
        assert_eq!(
            fixture_scope(&task_with_fixture("../../src/discovery")),
            vec!["src/discovery".to_string()]
        );
        assert_eq!(
            fixture_scope(&task_with_fixture("fixtures/repo")),
            vec!["benchmarks/accuracy/fixtures/repo".to_string()]
        );
    }

    /// Without a scope the question is repo-wide while the ground truth only holds
    /// inside the fixture, so a correct whole-repo answer scores wrong.
    #[test]
    fn agentic_prompt_carries_the_fixture_scope() {
        let p = format_agentic_user(&task_with_fixture("../../src/discovery"));
        assert!(p.contains("Scope: answer only about code under `src/discovery`"), "{p}");
        assert!(p.contains("Which function calls `spec_for_extension`?"), "{p}");
    }

    /// The baseline's isolation is structural: an empty server map plus
    /// `--strict-mcp-config`. `--allowedTools` does not gate a discoverable MCP
    /// server, so nothing here may depend on it.
    #[test]
    fn baseline_mcp_config_declares_no_servers_while_lens_config_declares_lens() {
        let baseline = json!({ "mcpServers": {} });
        assert!(baseline["mcpServers"].as_object().expect("map").is_empty());

        let lens = mcp_config_json(Path::new("/tmp/lens"));
        assert_eq!(lens["mcpServers"]["lens"]["command"], "/tmp/lens");
    }

    /// The baseline emulates a machine without lens. Its hooks cannot be
    /// un-merged, so `LENS_ROUTING=off` is what makes them contribute nothing.
    #[test]
    fn baseline_settings_turn_lens_off_entirely() {
        let s = baseline_settings_json();
        assert_eq!(s["env"]["LENS_ROUTING"], "off");
        assert_eq!(s["env"]["LENS_GREP_SCOPE_DENY"], "0");
        assert!(s.get("hooks").is_none(), "settings merge, so a hooks block is inert");
    }

    /// The lens arm is lens as shipped: routing at its default, rails untouched.
    /// Pinning the rails to "0" here would disable the adoption layer under test.
    #[test]
    fn lens_settings_ship_full_routing_with_rails_left_unset() {
        let s = lens_settings_json();
        assert_eq!(s["env"]["LENS_ROUTING"], "full");
        for flag in super::REROUTE_RAIL_FLAGS {
            assert!(
                s["env"].get(flag).is_none(),
                "{flag} must stay unset: it is a default-ON kill-switch"
            );
        }
    }

    /// The lens arm carries its own hooks, pointed at `lens_release_bin`, so it
    /// does not depend on whatever hooks happen to be installed globally (the
    /// 560e862 bug: hooks and `--mcp-config` disagreeing on the tool set).
    #[test]
    fn lens_settings_hooks_point_at_the_release_binary() {
        let s = lens_settings_json();
        let bin = super::lens_release_bin().to_string_lossy().to_string();
        for event in super::HOOK_EVENTS {
            let cmd = s["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap_or_else(|| panic!("{event} hook missing: {s}"));
            assert!(cmd.contains(&bin), "{event} hook does not point at the release binary: {cmd}");
            assert!(
                cmd.contains(&format!("hook claude {event}")),
                "{event} hook command malformed: {cmd}"
            );
        }
    }

    /// The validity gate. Two earlier builds passed every compile-time check and
    /// were still invalid (the baseline reached lens via `ToolSearch`; then both
    /// arms had routing amputated). Only a live session can prove the arms are the
    /// two configs a user could install, so this spawns real `claude -p` runs and
    /// is `#[ignore]`d to keep `cargo test` hermetic:
    ///
    ///   cargo test --bin bench_accuracy -- --ignored --nocapture arms_are_lens
    #[test]
    #[ignore = "spawns live `claude -p` sessions; needs target/release/lens built"]
    fn arms_are_lens_installed_vs_not() {
        use super::{arm_isolation, claude_agentic_attempt, is_lens_tool_name, AgenticHost};
        let iso = arm_isolation(AgenticHost::Claude).expect("isolation setup");
        let prompt = format_agentic_user(&task_with_fixture("../../src/discovery"));
        let model = super::default_model();

        let baseline = claude_agentic_attempt(&prompt, &model, &iso.baseline()).expect("baseline");
        eprintln!(
            "BASELINE  tools={:?} guide_injections={} turn_cap={}",
            baseline.tools, baseline.guide_injections, baseline.hit_turn_cap
        );
        let leaked: Vec<&String> = baseline
            .tools
            .iter()
            .filter(|t| is_lens_tool_name(t))
            .collect();
        assert!(leaked.is_empty(), "baseline reached lens: {leaked:?}");
        assert_eq!(
            baseline.guide_injections, 0,
            "baseline must look like lens was never installed"
        );

        let lens = claude_agentic_attempt(&prompt, &model, &iso.lens_arm()).expect("lens arm");
        eprintln!(
            "LENS ARM  tools={:?} guide_injections={} turn_cap={}",
            lens.tools, lens.guide_injections, lens.hit_turn_cap
        );
        assert!(
            lens.guide_injections >= 1,
            "lens arm got no SessionStart guide, so `full` routing did not take and the \
             arm is not lens-as-installed"
        );
        assert!(baseline.tokens > 0 && lens.tokens > 0, "both arms must report usage");
    }
}

#[cfg(test)]
mod validity_gate_tests {
    use super::{validate_arm_run, AgenticRun, ArmSpec, LENS_TOOLS};
    use std::path::Path;

    fn arm(expects_guide: bool) -> ArmSpec<'static> {
        ArmSpec {
            allowed_tools: LENS_TOOLS,
            mcp_config: Path::new("/tmp/mcp.json"),
            settings: Path::new("/tmp/settings.json"),
            expects_guide,
        }
    }

    fn run(guide_injections: usize, tools: Vec<&str>) -> AgenticRun {
        AgenticRun {
            answer: "{}".to_string(),
            lens_call_succeeded: tools.iter().any(|t| super::is_lens_tool_name(t)),
            tools: tools.into_iter().map(str::to_string).collect(),
            tokens: 10,
            guide_injections,
            hit_turn_cap: false,
            duration_ms: 100,
        }
    }

    /// The 560e862 failure mode (guide fired, zero organic `mcp__lens__*` calls)
    /// used to be REJECTED and dropped here. Post-canary it is a scored ADOPTION
    /// MISS: the canary gate upstream already proved the lens plumbing reaches
    /// lens, so this is genuine non-adoption, not broken config — `validate_arm_run`
    /// returns `Ok(true)` (score it, flag it) instead of deleting the cell.
    #[test]
    fn guide_fired_zero_lens_calls_scores_as_adoption_miss() {
        let r = run(1, vec!["Read", "Bash"]);
        let miss = validate_arm_run(&r, &arm(true)).expect("scored, not dropped");
        assert!(miss, "a canary-passed lens arm with zero lens calls is an adoption miss");
    }

    #[test]
    fn guide_fired_with_a_lens_call_is_valid_and_not_a_miss() {
        let r = run(1, vec!["Bash", "mcp__lens__lens_search"]);
        let miss = validate_arm_run(&r, &arm(true)).expect("valid");
        assert!(!miss, "a lens call is adoption, not a miss");
    }

    #[test]
    fn baseline_with_no_guide_and_no_lens_calls_is_valid() {
        let r = run(0, vec!["Read", "Bash"]);
        let miss = validate_arm_run(&r, &arm(false)).expect("valid");
        assert!(!miss, "the baseline arm is never an adoption miss");
    }

    #[test]
    fn missing_guide_on_a_lens_arm_is_rejected() {
        let r = run(0, vec!["mcp__lens__lens_search"]);
        let err = validate_arm_run(&r, &arm(true)).expect_err("must be invalid");
        assert!(err.contains("expected lens guide"), "{err}");
    }

    /// The 0079 failure mode: every usage source in the transcript was empty,
    /// the run parsed with tokens=0, and the old gate scored it — silently
    /// flattering the arm's token mean. Must be REJECTED, not scored.
    #[test]
    fn zero_token_run_is_rejected() {
        let mut r = run(1, vec!["mcp__lens__lens_search"]);
        r.tokens = 0;
        let err = validate_arm_run(&r, &arm(true)).expect_err("must be invalid");
        assert!(err.contains("tokens=0"), "{err}");
    }
}

#[cfg(test)]
mod function_report_tests {
    use super::{function_report, ArmResult, FunctionStats, Task, TaskResult};
    use serde_json::json;

    fn tagged_task(id: &str, lens_fn: &str) -> Task {
        serde_json::from_value(json!({
            "id": id,
            "prompt": "p",
            "fixtures": ["fixtures/repo"],
            "ground_truth": { "a": "b" },
            "check": "contains",
            "primary_mechanism": "search",
            "lens_fn": lens_fn,
            "evidence": ["b"],
            "treatment": { "queries": ["q"] }
        }))
        .expect("task spec")
    }

    fn arm(correct: bool, tokens: usize, millis: usize) -> ArmResult {
        ArmResult { correct, tokens, context_bytes: tokens, answer: json!({}), rounds: 1, millis, tools: vec![], adoption_miss: false }
    }

    fn result(id: &str, mechanism: &str, control: ArmResult, treatment: ArmResult) -> TaskResult {
        TaskResult { id: id.to_string(), mechanism: mechanism.to_string(), control, treatment, control_stats: None, treatment_stats: None, control_runs: vec![], treatment_runs: vec![] }
    }

    #[test]
    fn groups_by_lens_fn_and_folds_both_arms() {
        let tasks = vec![
            tagged_task("t1", "lens_search"),
            tagged_task("t2", "lens_search"),
            tagged_task("t3", "lens_symbol"),
        ];
        let results = vec![
            result("t1", "search", arm(false, 2000, 0), arm(true, 1000, 8000)),
            result("t2", "search", arm(false, 2000, 0), arm(true, 1400, 8400)),
            result("t3", "search", arm(true, 500, 0), arm(true, 300, 3000)),
        ];
        let (overall, per_function) = function_report(&tasks, &results);

        // overall = both arms folded across all 3 tagged tasks.
        assert_eq!(overall.len(), 2);
        let lens_overall = overall.iter().find(|r| r.arm == "lens").expect("lens row");
        assert_eq!(lens_overall.function, "overall");
        assert_eq!(lens_overall.n, 3);
        assert!((lens_overall.success_mean - 1.0).abs() < 1e-9);

        // per_function has one lens+baseline pair per distinct tag.
        let fns: std::collections::HashSet<&str> =
            per_function.iter().map(|r| r.function.as_str()).collect();
        assert_eq!(fns, std::collections::HashSet::from(["lens_search", "lens_symbol"]));
        let search_lens: &FunctionStats = per_function
            .iter()
            .find(|r| r.function == "lens_search" && r.arm == "lens")
            .expect("lens_search/lens row");
        assert_eq!(search_lens.n, 2);
        assert!((search_lens.tokens_mean - 1200.0).abs() < 1e-9, "{}", search_lens.tokens_mean);
        assert!((search_lens.duration_ms_mean - 8200.0).abs() < 1e-9, "{}", search_lens.duration_ms_mean);
    }

    #[test]
    fn untagged_tasks_do_not_participate() {
        let mut untagged = tagged_task("t1", "lens_search");
        untagged.lens_fn = None;
        let results = vec![result("t1", "search", arm(false, 2000, 0), arm(true, 1000, 8000))];
        let (overall, per_function) = function_report(&[untagged], &results);
        assert!(overall.iter().all(|r| r.n == 0));
        assert!(per_function.is_empty());
    }

    /// Contract shape check (Bench schema): `{fn, arm, n, success_mean,
    /// success_std, tokens_mean, tokens_std, duration_ms_mean,
    /// duration_ms_std}`, round-tripped through JSON without loss.
    #[test]
    fn function_stats_round_trips_through_json_with_contract_keys() {
        let row = FunctionStats {
            function: "lens_search".to_string(),
            arm: "lens".to_string(),
            n: 3,
            success_mean: 1.0,
            success_std: 0.0,
            tokens_mean: 1234.5,
            tokens_std: 45.2,
            duration_ms_mean: 8200.0,
            duration_ms_std: 300.1,
        };
        let v = serde_json::to_value(&row).expect("serialize");
        assert_eq!(v["fn"], "lens_search", "the Contract key is `fn`, not `function`: {v}");
        assert_eq!(v["arm"], "lens");
        assert_eq!(v["n"], 3);
        let back: FunctionStats = serde_json::from_value(v).expect("round trip");
        assert_eq!(back.function, row.function);
        assert_eq!(back.duration_ms_mean, row.duration_ms_mean);
        assert_eq!(back.duration_ms_std, row.duration_ms_std);
    }
}

#[cfg(test)]
mod back_compat_tests {
    use super::{accuracy_root, Group, TaskResult};
    use serde_json::Value;

    /// The committed results predate every K-run/agentic field, and both readers
    /// of them fail *silently*: `bench_report` falls back to a "not run yet" stub,
    /// and the harness's filtered-rerun merge does `.ok().unwrap_or_default()`,
    /// which drops every prior task instead of erroring. A new field that forgets
    /// `#[serde(default)]` still compiles, so pin the contract here.
    #[test]
    fn committed_results_still_deserialize() {
        for name in ["mock.json", "real.json"] {
            let path = accuracy_root().join("results").join(name);
            let raw = std::fs::read_to_string(&path).expect("read committed results");
            let doc: Value = serde_json::from_str(&raw).expect("results are json");
            let groups: Vec<Group> = serde_json::from_value(doc["groups"].clone())
                .unwrap_or_else(|e| panic!("{name} groups no longer deserialize: {e}"));
            let tasks: Vec<TaskResult> = serde_json::from_value(doc["tasks"].clone())
                .unwrap_or_else(|e| panic!("{name} tasks no longer deserialize: {e}"));
            assert!(!groups.is_empty(), "{name} lost its groups");
            assert!(!tasks.is_empty(), "{name} lost its tasks");
        }
    }
}

#[cfg(test)]
mod agentic_tests {
    use super::{parse_agentic_stream, stddev, stddev_of_sum, tool_use_names, usage_tokens};
    use serde_json::{json, Value};

    /// Shapes taken from a real `claude -p --output-format stream-json --verbose`
    /// run: one assistant message is split across lines (thinking, then tool_use)
    /// and repeats its partial `message.usage`, so per-turn usage is neither
    /// complete nor unique — only the `result` line's cumulative usage is.
    const FIXTURE: &str = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"hm"}],"usage":{"input_tokens":10,"output_tokens":4}}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}],"usage":{"input_tokens":10,"output_tokens":4}}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"mcp__lens__lens_links"}],"usage":{"input_tokens":6,"output_tokens":1}}}
not even json
{"type":"result","subtype":"success","is_error":false,"result":"{\"caller\":\"runtime_for\"}","usage":{"input_tokens":16,"cache_creation_input_tokens":63772,"cache_read_input_tokens":18653,"output_tokens":207}}"#;

    #[test]
    fn parses_answer_rounds_and_tokens_from_transcript() {
        let run = parse_agentic_stream(FIXTURE).expect("parse");
        assert_eq!(run.answer, "{\"caller\":\"runtime_for\"}");
        assert_eq!(run.rounds(), 2, "two distinct tool_use ids");
        assert_eq!(run.tools, vec!["Bash", "mcp__lens__lens_links"]);
        // Every input class plus output: 16 + 63772 + 18653 + 207.
        assert_eq!(run.tokens, 82648);
    }

    #[test]
    fn same_tool_use_id_across_streamed_lines_counts_once() {
        let dup = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read"}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read"}]}}"#;
        let lines: Vec<Value> = dup
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        assert_eq!(tool_use_names(&lines), vec!["Read"]);
    }

    #[test]
    fn is_error_result_is_an_error() {
        let stream = r#"{"type":"result","subtype":"error","is_error":true,"result":"boom"}"#;
        assert!(parse_agentic_stream(stream).is_err());
    }

    #[test]
    fn missing_result_line_is_an_error() {
        let stream = r#"{"type":"system","subtype":"init"}"#;
        assert!(parse_agentic_stream(stream).is_err());
    }

    /// `--max-turns` exhaustion (`is_error` + `error_max_turns`, no `.result`)
    /// is a scored failure — empty answer, cap flagged — not a parse error.
    #[test]
    fn max_turns_exhaustion_is_a_scored_failure_not_an_error() {
        let stream = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Grep"}]}}
{"type":"result","subtype":"error_max_turns","is_error":true,"duration_ms":25070,"usage":{"input_tokens":10,"output_tokens":5}}"#;
        let run = parse_agentic_stream(stream).expect("scored, not an error");
        assert!(run.hit_turn_cap);
        assert_eq!(run.answer, "");
        assert_eq!(run.rounds(), 1);
        assert_eq!(run.tokens, 15);
        assert_eq!(run.duration_ms, 25070, "wall-clock survives the cap path");
        assert_eq!(run.tools, vec!["Grep"], "tool sequence survives the cap path");
    }

    /// The guide sentinel only counts from `system` lines (hook output). An arm
    /// that Reads the source file defining the guide template echoes the
    /// sentinel through a tool_result `user` line, which must not count.
    #[test]
    fn guide_sentinel_in_tool_results_does_not_count_as_injection() {
        let stream = r#"{"type":"system","subtype":"hook_response","output":"<context_window_protection>guide</context_window_protection>"}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"const BLOCK_HEAD: &str = \"<context_window_protection>\";"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"{}","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let run = parse_agentic_stream(stream).expect("parse");
        assert_eq!(run.guide_injections, 1, "system line counts, tool_result does not");
    }

    /// A result envelope with no usage must not score as a zero-token session:
    /// fall back to the assistant messages' own usage, deduping the multi-line
    /// emissions of a single message by id (last emission wins).
    #[test]
    fn missing_result_usage_falls_back_to_assistant_usage() {
        let stream = r#"
{"type":"assistant","message":{"id":"m1","content":[{"type":"tool_use","id":"t1","name":"mcp__lens__lens_search","input":{}}],"usage":{"input_tokens":100,"output_tokens":10}}}
{"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"partial"}],"usage":{"input_tokens":100,"output_tokens":12}}}
{"type":"assistant","message":{"id":"m2","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":200,"cache_read_input_tokens":50,"output_tokens":5}}}
{"type":"result","subtype":"success","is_error":false,"result":"{\"function\":\"ensure_index\"}","duration_ms":100}"#;
        let run = parse_agentic_stream(stream).unwrap();
        assert_eq!(run.tokens, 112 + 255, "m1 deduped to last emission, plus m2");
    }

    /// Success requires a non-error `tool_result` correlated by `tool_use_id`:
    /// an instructed-but-refused lens call (the `--strict-mcp-config` baseline
    /// shape) must parse as attempted-not-succeeded.
    #[test]
    fn lens_call_success_requires_a_non_error_tool_result() {
        let refused = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"mcp__lens__lens_search","input":{}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":"No such tool available"}]}}
{"type":"result","subtype":"success","is_error":false,"result":"{}","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let run = parse_agentic_stream(refused).expect("parse");
        assert_eq!(run.tools, vec!["mcp__lens__lens_search"], "attempt recorded");
        assert!(!run.lens_call_succeeded, "errored result is not a success");

        let served = refused.replace(",\"is_error\":true", "");
        let run = parse_agentic_stream(&served).expect("parse");
        assert!(run.lens_call_succeeded, "non-error result is a success");

        // A non-lens success never counts.
        let other = refused.replace("mcp__lens__lens_search", "Bash");
        let run = parse_agentic_stream(&other).expect("parse");
        assert!(!run.lens_call_succeeded);
    }

    #[test]
    fn usage_counts_cached_input_not_just_the_uncached_remainder() {
        let usage = json!({
            "input_tokens": 16,
            "cache_creation_input_tokens": 63772,
            "cache_read_input_tokens": 18653,
            "output_tokens": 207,
        });
        assert_eq!(usage_tokens(&usage), 82648);
        // Absent classes are simply not counted, never fatal.
        assert_eq!(usage_tokens(&json!({ "input_tokens": 5 })), 5);
    }

    #[test]
    fn population_stddev() {
        assert_eq!(stddev(&[2.0, 4.0, 4.0, 4.0, 5.0, 5.0, 7.0, 9.0]), 2.0);
    }

    #[test]
    fn stddev_of_fewer_than_two_samples_is_zero() {
        assert_eq!(stddev(&[]), 0.0);
        assert_eq!(stddev(&[7.0]), 0.0);
    }

    #[test]
    fn stddev_of_a_sum_adds_variances_not_deviations() {
        // 3² + 4² = 5², not 3 + 4.
        assert_eq!(stddev_of_sum([3.0, 4.0].into_iter()), 5.0);
    }
}

#[cfg(test)]
mod canary_set_adoption_tests {
    use super::{
        adoption_report, canary_gate, canary_verdict, filter_tasks, load_task_set, load_tasks,
        score_gated, set_label, AgenticRun, ArmResult, ArmSpec, Model, Task, TaskResult, LENS_TOOLS,
    };
    use serde_json::json;
    use std::path::Path;

    fn agentic_run(tools: Vec<&str>) -> AgenticRun {
        AgenticRun {
            answer: "{}".to_string(),
            lens_call_succeeded: tools.iter().any(|t| super::is_lens_tool_name(t)),
            tools: tools.into_iter().map(str::to_string).collect(),
            tokens: 10,
            guide_injections: 1,
            hit_turn_cap: false,
            duration_ms: 100,
        }
    }

    fn arm(expects_guide: bool) -> ArmSpec<'static> {
        ArmSpec {
            allowed_tools: LENS_TOOLS,
            mcp_config: Path::new("/tmp/mcp.json"),
            settings: Path::new("/tmp/settings.json"),
            expects_guide,
        }
    }

    fn cell(tools: Vec<&str>, adoption_miss: bool) -> ArmResult {
        ArmResult {
            correct: true,
            tokens: 10,
            context_bytes: 1,
            answer: json!({}),
            rounds: tools.len(),
            millis: 1,
            tools: tools.into_iter().map(str::to_string).collect(),
            adoption_miss,
        }
    }

    fn task(id: &str, mech: &str) -> Task {
        serde_json::from_value(json!({
            "id": id, "prompt": "p", "fixtures": ["fixtures/repo"],
            "ground_truth": { "a": "b" }, "check": "contains",
            "primary_mechanism": mech, "evidence": ["b"],
            "treatment": { "queries": ["q"] }
        }))
        .expect("task spec")
    }

    fn path_task(id: &str) -> Task {
        serde_json::from_value(json!({
            "id": id, "prompt": "reach?", "fixtures": ["fixtures/repo"],
            "ground_truth": { "reachable": "yes" }, "check": "contains",
            "primary_mechanism": "discovery", "evidence": ["connect_db"],
            "treatment": { "graph_op": "path", "from": "handle_request", "to": "connect_db" }
        }))
        .expect("task spec")
    }

    // --- canary verdict per (model, ArmSpec) --------------------------------

    #[test]
    fn lens_arm_canary_passes_only_when_lens_is_reached() {
        assert!(canary_verdict(&agentic_run(vec!["mcp__lens__lens_search"]), &arm(true)));
        assert!(!canary_verdict(&agentic_run(vec!["Read", "Bash"]), &arm(true)));
    }

    #[test]
    fn baseline_arm_canary_passes_only_when_lens_is_not_reached() {
        assert!(canary_verdict(&agentic_run(vec!["Read", "Bash"]), &arm(false)));
        assert!(!canary_verdict(&agentic_run(vec!["mcp__lens__lens_search"]), &arm(false)));
    }

    #[test]
    fn canary_keys_on_call_success_not_attempt() {
        // An attempted-but-FAILED lens call: the baseline's isolation held
        // (pass), and the lens arm's plumbing did NOT work (fail).
        let mut attempted = agentic_run(vec!["mcp__lens__lens_search"]);
        attempted.lens_call_succeeded = false;
        assert!(
            canary_verdict(&attempted, &arm(false)),
            "an obedient baseline model attempting a call that errors is proof \
             isolation held, not a leak"
        );
        assert!(
            !canary_verdict(&attempted, &arm(true)),
            "a lens arm whose forced call errors has broken plumbing"
        );
    }

    // --- canary gate aborts the suite ---------------------------------------

    #[test]
    fn canary_gate_aborts_on_fail_and_proceeds_on_pass() {
        assert!(canary_gate(false).is_err(), "a failed canary must abort the suite");
        assert!(canary_gate(true).is_ok());
    }

    /// The core protection: a failed canary yields NO scored cells (the prior run
    /// silently dropped them; now the whole suite refuses to score). A passing
    /// canary scores normally.
    #[tokio::test]
    async fn canary_fail_produces_no_scored_cells_pass_scores_normally() {
        let t = path_task("t_gate");
        let aborted = score_gated(std::slice::from_ref(&t), &Model::Mock, 1, false).await;
        assert!(aborted.is_err(), "canary fail = abort, no scored cells");
        let scored = score_gated(std::slice::from_ref(&t), &Model::Mock, 1, true)
            .await
            .expect("canary pass scores");
        assert_eq!(scored.len(), 1, "canary pass scores the task normally");
    }

    // --- adoption metric ----------------------------------------------------

    /// Canary-pass + a zero-lens lens-arm run scores normally AND flags
    /// `adoption_miss`, and the per-model `adoption_rate` reflects it.
    #[test]
    fn zero_lens_run_scores_as_adoption_miss_and_lowers_the_rate() {
        // Two single-run treatment cells: one reached lens, one did not.
        let adopted = TaskResult {
            id: "a".into(),
            mechanism: "search".into(),
            control: cell(vec![], false),
            treatment: cell(vec!["mcp__lens__lens_search"], false),
            control_stats: None,
            treatment_stats: None,
            control_runs: vec![],
            treatment_runs: vec![],
        };
        let missed = TaskResult {
            id: "b".into(),
            mechanism: "search".into(),
            control: cell(vec![], false),
            treatment: cell(vec![], true), // scored, flagged as a miss (not dropped)
            control_stats: None,
            treatment_stats: None,
            control_runs: vec![],
            treatment_runs: vec![],
        };
        let a = adoption_report(&[adopted, missed]);
        assert_eq!(a.total_runs, 2, "both cells were scored, neither dropped");
        assert_eq!(a.lens_runs, 1);
        assert_eq!(a.adoption_misses, 1);
        assert!((a.adoption_rate - 0.5).abs() < 1e-9);
    }

    #[test]
    fn adoption_miss_serializes_only_when_true() {
        let mut c = cell(vec![], true);
        assert_eq!(serde_json::to_value(&c).unwrap()["adoption_miss"], json!(true));
        c.adoption_miss = false;
        assert!(
            serde_json::to_value(&c).unwrap().get("adoption_miss").is_none(),
            "false must be skipped so pre-canary results serialize byte-identically"
        );
    }

    // --- frozen set filter + stamp ------------------------------------------

    #[test]
    fn set_filter_keeps_exactly_the_listed_ids() {
        let tasks = vec![task("0060_x", "search"), task("0061_y", "discovery"), task("0099_z", "search")];
        let set = vec!["0060_x".to_string(), "0061_y".to_string()];
        let ids: Vec<String> = filter_tasks(tasks, Some(&set), None)
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(ids, vec!["0060_x".to_string(), "0061_y".to_string()]);
    }

    #[test]
    fn set_and_only_compose_as_intersection_not_replacement() {
        let tasks = vec![task("0060_x", "search"), task("0061_y", "discovery")];
        let set = vec!["0060_x".to_string(), "0061_y".to_string()];
        let ids: Vec<String> = filter_tasks(tasks, Some(&set), Some("discovery"))
            .iter()
            .map(|t| t.id.clone())
            .collect();
        assert_eq!(ids, vec!["0061_y".to_string()], "both filters apply together");
    }

    #[test]
    fn set_label_is_the_stamped_file_stem() {
        assert_eq!(set_label("sets/0.10.json"), "0.10");
        assert_eq!(set_label("/abs/0.11-dev.json"), "0.11-dev");
    }

    /// The committed frozen set loads, is exactly the 22 real_agentic ids, and
    /// filtering the live task list by it yields those 22 tasks — the stamp
    /// (`set_label`) and the filter agree on the same set.
    #[test]
    fn frozen_010_set_is_the_22_real_agentic_tasks() {
        let ids = load_task_set("sets/0.10.json").expect("load 0.10 set");
        assert_eq!(ids.len(), 22, "0.10 is the frozen 22-task set");
        let kept = filter_tasks(load_tasks().expect("tasks"), Some(&ids), None);
        assert_eq!(kept.len(), 22, "every set id resolves to a live task");
        assert!(
            kept.iter().all(|t| t.id.contains("real_agentic")),
            "the frozen set is the agentic A/B tasks"
        );
    }
}
