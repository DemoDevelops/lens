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
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use lens::darkroom;
use lens::discovery::{self, query as gquery};
use lens::index::Index;
use lens::store::Store;
use lens::tools::ExecuteRequest;

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
}

impl Model {
    pub fn label(&self) -> String {
        match self {
            Model::Mock => "mock".to_string(),
            Model::Anthropic(m) => m.clone(),
            Model::ClaudePty(m) if m.is_empty() => "claude-pty".to_string(),
            Model::ClaudePty(m) => format!("{m} (via claude-pty)"),
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub id: String,
    pub mechanism: String,
    pub control: ArmResult,
    pub treatment: ArmResult,
}

/// Run both arms of one task.
pub async fn run_task(task: &Task, model: &Model) -> anyhow::Result<TaskResult> {
    let control_ctx = build_control_context(task)?;
    let treatment_ctx = build_treatment_context(task).await?;

    let control = run_arm(task, model, &control_ctx)?;
    let treatment = run_arm(task, model, &treatment_ctx)?;

    Ok(TaskResult {
        id: task.id.clone(),
        mechanism: task.primary_mechanism.clone(),
        control,
        treatment,
    })
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
    };
    let correct = score(&answer, &task.ground_truth, &task.check, task.tolerance);
    Ok(ArmResult {
        correct,
        tokens: est_tokens(context),
        context_bytes: context.len(),
        answer,
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
        let index = Index::open(data.path())?;
        for f in &task.fixtures {
            index.index_path(&accuracy_root().join(f), true)?;
        }
        let resp = index.search(queries, 5)?;
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
            // The token-budgeted repomap; budget matches the lens_overview tool default.
            "overview" => gquery::overview(&outcome.graph, 2000),
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
        let skel = discovery::skeleton::skeletonize(&content, &spec)
            .ok_or_else(|| anyhow::anyhow!("could not skeletonize {}", p.display()))?;
        return Ok(skel);
    }
    Err(anyhow::anyhow!("task {} has no treatment spec", task.id))
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

fn format_user(context: &str, prompt: &str, ground_truth: &Value) -> String {
    let keys: Vec<String> = ground_truth
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
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
        let control_correct = rows.iter().filter(|r| r.control.correct).count();
        let treat_correct = rows.iter().filter(|r| r.treatment.correct).count();
        groups.push(Group {
            mechanism: mech.to_string(),
            n,
            control_acc: control_correct as f64 / n as f64,
            treatment_acc: treat_correct as f64 / n as f64,
            control_tokens: rows.iter().map(|r| r.control.tokens).sum(),
            treatment_tokens: rows.iter().map(|r| r.treatment.tokens).sum(),
        });
    }
    groups
}

/// Render the accuracy table (§4.4 of the plan). `model_label` names the arm's
/// model; `pending` true means no real-model run has happened yet.
pub fn render_accuracy_markdown(groups: &[Group], model_label: &str, pending: bool) -> String {
    let mut s = String::new();
    if pending {
        s.push_str("> **Accuracy: pending real-model run.** The numbers below are from the mock oracle (a context-presence stub that tests scoring/plumbing only). Set `ANTHROPIC_API_KEY` and re-run `bench_accuracy` for real-model results.\n\n");
    }
    s.push_str(&format!("Model: `{model_label}`\n\n"));
    s.push_str("| Task set | N | Control acc | lens acc | Δ acc | Control tokens | lens tokens | Token Δ |\n");
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");
    let mut neg = Vec::new();
    for g in groups {
        let delta = g.treatment_acc - g.control_acc;
        if delta < -0.0001 {
            neg.push(g.mechanism.clone());
        }
        s.push_str(&format!(
            "| {} tasks | {} | {:.0}% | {:.0}% | {:+.0}pp | {} | {} | {:+} |\n",
            cap(&g.mechanism),
            g.n,
            g.control_acc * 100.0,
            g.treatment_acc * 100.0,
            delta * 100.0,
            g.control_tokens,
            g.treatment_tokens,
            g.treatment_tokens as i64 - g.control_tokens as i64,
        ));
    }
    if !neg.is_empty() {
        s.push_str(&format!(
            "\n⚠️ **Negative accuracy delta on: {}.** A mechanism that loses accuracy is dropping load-bearing context and needs fixing or scoping.\n",
            neg.join(", ")
        ));
    }
    s
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
