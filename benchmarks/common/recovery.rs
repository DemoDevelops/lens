//! Shared session-recovery benchmark logic, `#[path]`-included by the recovery
//! harness and the report generator.
//!
//! The bar here is **Context Mode's behavior**, not lens's own sense of
//! working. Each scenario establishes a working state (file edits, tasks, an
//! error, a user decision, git ops), forces a compaction boundary, then poses a
//! follow-up that is *only* answerable if the working state survived. We run it
//! through three isolated arms and score survival against ground truth.
//!
//!   * **No continuity** (floor): nothing survives the compaction.
//!   * **Context Mode** (the bar): its real hook scripts build the snapshot.
//!   * **lens** (the candidate): its session pipeline builds the snapshot.
//!
//! Mock-model mode (a context-presence oracle) tests the plumbing with no API
//! key; a real-model run happens when `ANTHROPIC_API_KEY` is set.

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use lens::session::{extract, snapshot, store::SessionStore};

/// Accurate token count via the offline o200k_base BPE (replaces the old bytes/4
/// heuristic), so the recovered-context token figures reflect real tokenization.
pub fn est_tokens(text: &str) -> usize {
    lens::obs::count_tokens(text)
}

pub fn recovery_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/recovery")
}

pub fn default_model() -> String {
    std::env::var("LENS_BENCH_MODEL").unwrap_or_else(|_| "claude-haiku-4-5".to_string())
}

// --- Scenario spec ----------------------------------------------------------

/// One simulated lifecycle step before the compaction boundary.
#[derive(Debug, Clone, Deserialize)]
pub struct Step {
    /// "UserPromptSubmit" or "PostToolUse".
    pub hook: String,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_input: Option<Value>,
    #[serde(default)]
    pub tool_response: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scenario {
    pub id: String,
    /// Grouping for the recovery table: "file_task" or "error_decision".
    pub set: String,
    pub steps: Vec<Step>,
    pub followup: String,
    pub ground_truth: Value,
    pub check: String,
    /// Substrings that must survive the compaction for the answer to be
    /// derivable (the mock oracle's survival test).
    pub evidence: Vec<String>,
}

pub fn load_scenarios() -> anyhow::Result<Vec<Scenario>> {
    let dir = recovery_root().join("scenarios");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "json").unwrap_or(false))
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for p in paths {
        let raw = std::fs::read_to_string(&p)?;
        let s: Scenario = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", p.display()))?;
        out.push(s);
    }
    Ok(out)
}

// --- Arms -------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm {
    NoContinuity,
    ContextMode,
    Ctxforge,
}

impl Arm {
    pub fn key(&self) -> &'static str {
        match self {
            Arm::NoContinuity => "no_continuity",
            Arm::ContextMode => "context_mode",
            Arm::Ctxforge => "lens",
        }
    }
}

/// Build the context a resumed session would receive for `arm`. `Ok(None)`
/// means the arm is unavailable (e.g. Context Mode not runnable here).
pub fn recover(scenario: &Scenario, arm: Arm) -> anyhow::Result<Option<String>> {
    match arm {
        Arm::NoContinuity => Ok(Some(String::new())),
        Arm::Ctxforge => Ok(Some(lens_recover(scenario)?)),
        Arm::ContextMode => context_mode_recover(scenario),
    }
}

/// lens arm: drive the scenario through the real session pipeline
/// (extract → store → snapshot), exactly as the PreCompact hook would.
fn lens_recover(scenario: &Scenario) -> anyhow::Result<String> {
    let data = tempfile::tempdir()?;
    let store = SessionStore::open(data.path())?;
    let sid = "bench";
    let proj = "/bench";
    for (i, step) in scenario.steps.iter().enumerate() {
        let ts = i as i64 + 1;
        let raws = match step.hook.as_str() {
            "UserPromptSubmit" => {
                extract::extract_user_events(step.prompt.as_deref().unwrap_or(""))
            }
            "PostToolUse" => extract::extract_tool_events(
                step.tool_name.as_deref().unwrap_or(""),
                step.tool_input.as_ref().unwrap_or(&json!({})),
                step.tool_response.as_deref().unwrap_or(""),
            ),
            _ => Vec::new(),
        };
        let events: Vec<_> = raws
            .into_iter()
            .map(|r| r.attribute(sid, proj, ts, &step.hook))
            .collect();
        store.insert_events(&events)?;
    }
    let events = store.events_for_session(sid)?;
    Ok(snapshot::build_snapshot(
        &events,
        lens::session::snapshot_budget(),
        1,
    ))
}

/// Resolve Context Mode's hooks dir: `$CONTEXT_MODE_HOOKS_DIR`, else the newest
/// version under the plugin cache.
pub fn context_mode_hooks_dir() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CONTEXT_MODE_HOOKS_DIR") {
        let p = PathBuf::from(d);
        if p.join("sessionstart.mjs").exists() {
            return Some(p);
        }
    }
    let home = std::env::var_os("HOME")?;
    let base = PathBuf::from(home).join(".claude/plugins/cache/context-mode/context-mode");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("hooks/sessionstart.mjs").exists())
        .collect();
    versions.sort();
    versions.pop().map(|v| v.join("hooks"))
}

fn bun_available() -> bool {
    Command::new("bun")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Context Mode arm: run the scenario through its real hook scripts and read the
/// `additionalContext` its SessionStart(compact) injects. `Ok(None)` if Context
/// Mode is not runnable (bun missing or plugin absent) — we never fake the bar.
fn context_mode_recover(scenario: &Scenario) -> anyhow::Result<Option<String>> {
    let hooks = match context_mode_hooks_dir() {
        Some(h) if bun_available() => h,
        _ => return Ok(None),
    };
    // Unique project dir per scenario isolates Context Mode's per-project DB.
    let proj = tempfile::tempdir()?;
    let proj_str = proj.path().to_string_lossy().to_string();
    let sid = format!("cm-{}", scenario.id);

    let run_script = |script: &str, payload: &Value| -> anyhow::Result<String> {
        let path = hooks.join(script);
        let mut child = Command::new("bun")
            .arg(&path)
            .current_dir(proj.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())?;
        let out = child.wait_with_output()?;
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    };

    for step in &scenario.steps {
        let mut p = json!({ "session_id": sid, "cwd": proj_str });
        let script = match step.hook.as_str() {
            "UserPromptSubmit" => {
                p["prompt"] = json!(step.prompt.clone().unwrap_or_default());
                "userpromptsubmit.mjs"
            }
            "PostToolUse" => {
                p["tool_name"] = json!(step.tool_name.clone().unwrap_or_default());
                p["tool_input"] = step.tool_input.clone().unwrap_or(json!({}));
                p["tool_response"] = json!(step.tool_response.clone().unwrap_or_default());
                "posttooluse.mjs"
            }
            _ => continue,
        };
        let _ = run_script(script, &p);
    }
    // Compaction boundary.
    let pc = json!({ "session_id": sid, "cwd": proj_str });
    let _ = run_script("precompact.mjs", &pc);
    // Resume after compaction.
    let ss = json!({ "session_id": sid, "cwd": proj_str, "source": "compact" });
    let out = run_script("sessionstart.mjs", &ss)?;
    let v: Value = serde_json::from_str(out.trim()).unwrap_or(json!({}));
    let ctx = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_string();
    Ok(Some(ctx))
}

// --- Model ------------------------------------------------------------------

pub enum Model {
    Mock,
    Anthropic(String),
    /// Real model via `claude-pty` (interactive Claude Code, plan quota). Tools
    /// disabled so the model answers only from the recovered context. The string
    /// is the `--model` passed to `claude-pty` (empty = session default).
    ClaudePty(String),
}

impl Model {
    pub fn label(&self) -> String {
        match self {
            Model::Mock => "mock".into(),
            Model::Anthropic(m) => m.clone(),
            Model::ClaudePty(m) if m.is_empty() => "claude-pty".into(),
            Model::ClaudePty(m) => format!("{m} (via claude-pty)"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ArmOutcome {
    pub arm: String,
    /// None = arm unavailable (not runnable here).
    pub available: bool,
    pub survived: bool,
    pub tokens: usize,
    pub context_bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioResult {
    pub id: String,
    pub set: String,
    pub arms: Vec<ArmOutcome>,
}

pub fn run_scenario(scenario: &Scenario, model: &Model) -> anyhow::Result<ScenarioResult> {
    let mut arms = Vec::new();
    for arm in [Arm::NoContinuity, Arm::ContextMode, Arm::Ctxforge] {
        let ctx = recover(scenario, arm)?;
        let outcome = match ctx {
            None => ArmOutcome {
                arm: arm.key().into(),
                available: false,
                survived: false,
                tokens: 0,
                context_bytes: 0,
            },
            Some(ctx) => {
                let answer = answer(model, &ctx, scenario);
                let survived = score(&answer, &scenario.ground_truth, &scenario.check);
                ArmOutcome {
                    arm: arm.key().into(),
                    available: true,
                    survived,
                    tokens: est_tokens(&ctx),
                    context_bytes: ctx.len(),
                }
            }
        };
        arms.push(outcome);
    }
    Ok(ScenarioResult {
        id: scenario.id.clone(),
        set: scenario.set.clone(),
        arms,
    })
}

fn answer(model: &Model, ctx: &str, scenario: &Scenario) -> Value {
    match model {
        Model::Mock => mock_answer(ctx, &scenario.evidence, &scenario.ground_truth),
        Model::Anthropic(id) => {
            let user = format_user(ctx, &scenario.followup, &scenario.ground_truth);
            match call_anthropic(id, SYSTEM_PROMPT, &user) {
                Ok(raw) => extract_json(&raw),
                Err(e) => {
                    eprintln!("anthropic call failed: {e}");
                    json!({})
                }
            }
        }
        Model::ClaudePty(m) => {
            let user = format_user(ctx, &scenario.followup, &scenario.ground_truth);
            match call_claude_pty(m, SYSTEM_PROMPT, &user) {
                Ok(obj) => extract_json(&obj),
                Err(e) => {
                    eprintln!("claude-pty call failed: {e}");
                    json!({})
                }
            }
        }
    }
}

/// Survival oracle: the answer is derivable iff every evidence token survived
/// in the recovered context.
pub fn mock_answer(ctx: &str, evidence: &[String], ground_truth: &Value) -> Value {
    let lc = ctx.to_ascii_lowercase();
    let derivable = !evidence.is_empty()
        && evidence
            .iter()
            .all(|e| lc.contains(&e.to_ascii_lowercase()));
    if derivable {
        ground_truth.clone()
    } else {
        let mut m = Map::new();
        if let Some(o) = ground_truth.as_object() {
            for k in o.keys() {
                m.insert(k.clone(), json!("UNKNOWN"));
            }
        }
        Value::Object(m)
    }
}

const SYSTEM_PROMPT: &str = "You are resuming a software session after the conversation was compacted. Answer strictly from the provided Session Guide / context. If the context does not contain the answer, respond with the value \"UNKNOWN\" for that key. Respond with a single minified JSON object and nothing else.";

fn format_user(ctx: &str, followup: &str, gt: &Value) -> String {
    let keys: Vec<String> = gt
        .as_object()
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default();
    format!(
        "Recovered context after compaction:\n{ctx}\n\nFollow-up question: {followup}\n\nRespond with ONLY a JSON object with exactly these keys: {keys:?}."
    )
}

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

/// Drive a real model through `claude-pty` (plan quota, not API credit). Tools
/// disabled and run in the trusted project dir, so the model answers only from
/// the recovered context — same isolation as the `curl` arm. Returns the last
/// balanced `{...}` from the screen scrape (the answer trails the prompt echo).
fn call_claude_pty(model: &str, system: &str, user: &str) -> Result<String, String> {
    let prompt = format!("{system}\n\n{user}");
    let workdir = env!("CARGO_MANIFEST_DIR");
    let mut cmd = Command::new("claude-pty");
    cmd.args(["--working-dir", workdir])
        .args(["--allowed-tools", ""])
        .args(["--effort", "low"])
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
    last_json_object(&stdout).ok_or_else(|| {
        format!(
            "claude-pty produced no JSON object (status {}, stderr: {})",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

/// Extract the last balanced `{...}` object from `s` (robust to the prompt echo
/// that precedes the model's trailing answer object).
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
                    return Some(chars[i as usize..=end].iter().collect());
                }
            }
            _ => {}
        }
        i -= 1;
    }
    None
}

fn extract_json(raw: &str) -> Value {
    let (start, end) = (raw.find('{'), raw.rfind('}'));
    if let (Some(s), Some(e)) = (start, end) {
        if e > s {
            if let Ok(v) = serde_json::from_str::<Value>(&raw[s..=e]) {
                return v;
            }
        }
    }
    json!({})
}

pub fn score(answer: &Value, ground_truth: &Value, check: &str) -> bool {
    let gt = match ground_truth.as_object() {
        Some(o) => o,
        None => return false,
    };
    for (k, expected) in gt {
        let got = answer.get(k);
        let ok = match check {
            "exact_match" => exact_eq(got, expected),
            _ => contains_eq(got, expected),
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

fn exact_eq(got: Option<&Value>, expected: &Value) -> bool {
    got.map(|g| {
        value_str(g)
            .trim()
            .eq_ignore_ascii_case(value_str(expected).trim())
    })
    .unwrap_or(false)
}

fn contains_eq(got: Option<&Value>, expected: &Value) -> bool {
    got.map(|g| {
        value_str(g)
            .to_ascii_lowercase()
            .contains(&value_str(expected).to_ascii_lowercase())
    })
    .unwrap_or(false)
}

// --- Aggregation + rendering ------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub set: String,
    pub n: usize,
    pub no_continuity: f64,
    pub context_mode: Option<f64>,
    pub lens: f64,
    pub cm_available: usize,
    pub cm_tokens: usize,
    pub lens_tokens: usize,
}

fn survival(results: &[&ScenarioResult], arm: &str) -> (usize, usize, usize) {
    // returns (survived, available, total_tokens) for the arm
    let mut survived = 0;
    let mut available = 0;
    let mut tokens = 0;
    for r in results {
        if let Some(a) = r.arms.iter().find(|a| a.arm == arm) {
            if a.available {
                available += 1;
                tokens += a.tokens;
                if a.survived {
                    survived += 1;
                }
            }
        }
    }
    (survived, available, tokens)
}

pub fn aggregate(results: &[ScenarioResult]) -> Vec<Group> {
    let order = ["file_task", "error_decision"];
    let mut groups = Vec::new();
    for set in order {
        let rows: Vec<&ScenarioResult> = results.iter().filter(|r| r.set == set).collect();
        if rows.is_empty() {
            continue;
        }
        let n = rows.len();
        let (nc_s, _nc_a, _) = survival(&rows, "no_continuity");
        let (cm_s, cm_a, cm_tok) = survival(&rows, "context_mode");
        let (cf_s, _cf_a, cf_tok) = survival(&rows, "lens");
        groups.push(Group {
            set: set.to_string(),
            n,
            no_continuity: nc_s as f64 / n as f64,
            context_mode: if cm_a > 0 {
                Some(cm_s as f64 / cm_a as f64)
            } else {
                None
            },
            lens: cf_s as f64 / n as f64,
            cm_available: cm_a,
            cm_tokens: cm_tok,
            lens_tokens: cf_tok,
        });
    }
    groups
}

pub fn render_recovery_markdown(groups: &[Group], model_label: &str, pending: bool) -> String {
    let mut s = String::new();
    if pending {
        s.push_str("> **Recovery: pending real-model run.** Numbers below are from the mock survival oracle (context-presence; tests plumbing only). Set `ANTHROPIC_API_KEY` and re-run `bench_recovery` for real-model results.\n\n");
    }
    s.push_str(&format!("Model: `{model_label}`. Survival = % of scenarios whose working state was recoverable from the post-compaction context.\n\n"));
    s.push_str("| Scenario set | N | No-continuity | Context Mode | lens | Δ (lens − CM) | CM tokens | lens tokens |\n");
    s.push_str("| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n");

    let cm_unavailable = groups.iter().any(|g| g.context_mode.is_none());
    let mut regressions = Vec::new();
    for g in groups {
        let cm_cell = match g.context_mode {
            Some(v) => format!("{:.0}%", v * 100.0),
            None => "n/a".to_string(),
        };
        let delta_cell = match g.context_mode {
            Some(v) => {
                let d = g.lens - v;
                if d < -0.0001 {
                    regressions.push(g.set.clone());
                }
                format!("{:+.0}pp", d * 100.0)
            }
            None => "n/a".to_string(),
        };
        let cm_tok = if g.cm_available > 0 {
            g.cm_tokens.to_string()
        } else {
            "n/a".into()
        };
        s.push_str(&format!(
            "| {} | {} | {:.0}% | {} | {:.0}% | {} | {} | {} |\n",
            label(&g.set),
            g.n,
            g.no_continuity * 100.0,
            cm_cell,
            g.lens * 100.0,
            delta_cell,
            cm_tok,
            g.lens_tokens,
        ));
    }

    s.push('\n');
    if cm_unavailable {
        s.push_str("⚠️ **Context Mode arm unavailable** (bun and/or the context-mode plugin not runnable here), so the bar could not be measured for some rows. Install bun + context-mode, or set `CONTEXT_MODE_HOOKS_DIR`, and re-run for the head-to-head.\n");
    }
    if !regressions.is_empty() {
        s.push_str(&format!(
            "❌ **lens underperforms Context Mode on: {}.** Do not rely on the swap for these until fixed.\n",
            regressions.join(", ")
        ));
    } else if !cm_unavailable && !groups.is_empty() {
        s.push_str("✅ **lens ≥ Context Mode** on every scenario set above — the swap is safe on recovery fidelity.\n");
    }
    s
}

fn label(set: &str) -> &str {
    match set {
        "file_task" => "File/task recovery",
        "error_decision" => "Error/decision recovery",
        other => other,
    }
}
