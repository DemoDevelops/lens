//! Rail 3 — offline intent-judge miner.
//!
//! Measures, per lens tool, how often the agent reached for a worse *primitive*
//! (Read / Grep / Bash / Edit) when a lens graph/structure tool would have fit
//! the decision better. It replays real Claude Code session transcripts, samples
//! the assistant turns that chose a non-lens tool, and asks a headless judge
//! which lens tool (if any) each decision should have used. The verdicts feed a
//! per-tool miss-rate and a set of proposed `mined_cases`-shaped precedents.
//!
//! Unlike `run_toolsel` (which spawns a live `claude` + lens MCP server to
//! measure behaviour going forward), this is a backward-looking metric over
//! transcripts already on disk — the offline backbone Rail 3 is scored against.
//!
//!   cargo run --bin run_intent_judge -- --n 20 --mock   # deterministic, no quota
//!   cargo run --bin run_intent_judge -- --n 40          # live judge (claude-pty/claude -p)
//!
//! Resumable: the checkpoint IS `results/intent_miss.json`. Each run judges up to
//! `--n` *new* decisions (sorted by turn-id), merges them with the already-judged
//! set, and re-aggregates. Re-runs skip judged turn-ids and only extend.

use std::collections::{BTreeMap, VecDeque};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- verdict space -----------------------------------------------------------

/// The five lens tools the miss-rate is aggregated over (the Rail-3 metric).
/// `lens_overview` / `lens_find` are valid judge verdicts too but are recorded
/// only in the raw `verdicts` tally, not the per-tool miss-rate map.
const AGG_TOOLS: [&str; 5] = [
    "lens_symbol",
    "lens_links",
    "lens_path",
    "lens_grep_ast",
    "lens_skeleton",
];

/// Every token the judge is allowed to emit (used to parse a live verdict).
const KNOWN_VERDICTS: [&str; 9] = [
    "lens_symbol",
    "lens_links",
    "lens_path",
    "lens_grep_ast",
    "lens_skeleton",
    "lens_overview",
    "lens_find",
    "none",
    "reason-instead-of-query",
];

/// Denominator definition (STABLE — do not change without re-baselining).
///
/// For a lens tool `T`, `addressable(T)` is the set of *chosen primitives* for
/// which a verdict of `T` is a plausible reroute — i.e. the primitive calls `T`
/// competes with. The per-tool miss-rate is then:
///
///   den(T) = # judged decisions whose chosen tool ∈ addressable(T)
///   num(T) = # of those whose judged verdict == T
///   miss_rate(T) = num(T) / den(T)          (0.0 when den(T) == 0)
///
/// `num(T)` is restricted to the denominator on purpose, so the rate stays in
/// [0,1] and reads as "of the primitive calls T could have won, the fraction the
/// judge says T should actually have won". Denominators intentionally overlap
/// (Grep is addressable by symbol/links/path/grep_ast; Read by symbol/links/
/// path/skeleton) — each rate is the capture share of that shared pool.
/// Bash/Edit/Glob decisions are judged and tallied but belong to no
/// denominator, so they never distort a rate.
fn addressable(tool: &str) -> &'static [&'static str] {
    match tool {
        "lens_symbol" | "lens_links" | "lens_path" => &["Grep", "Read"],
        "lens_grep_ast" => &["Grep"],
        "lens_skeleton" => &["Read"],
        _ => &[],
    }
}

// --- transcript location -----------------------------------------------------

/// Directory of Claude Code session transcripts for this repo. Overridable via
/// `LENS_INTENT_TRANSCRIPT_DIR` (used by nothing in prod; handy for pointing the
/// miner at a fixture tree). Default matches the project-scoped path Claude Code
/// writes to.
fn transcript_dir() -> PathBuf {
    if let Ok(d) = std::env::var("LENS_INTENT_TRANSCRIPT_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude-personal/projects/-Users-gene-Documents-AI-Stuff-lens")
}

fn results_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/toolsel/results")
}

fn judge_prompt_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/toolsel/judge_prompt.txt")
}

/// Minimal embedded transcript so `--mock` always produces output even when the
/// real transcript dir is absent/empty (CI). One session, three decisions.
const EMBEDDED_FIXTURE: &str = r#"{"type":"user","sessionId":"cifixture01","message":{"role":"user","content":"Where is the throttle helper `fired` defined?"}}
{"type":"assistant","sessionId":"cifixture01","uuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"cf1","name":"Grep","input":{"pattern":"fired","output_mode":"content"}}]}}
{"type":"user","sessionId":"cifixture01","message":{"role":"user","content":"Now show me the shape of src/session/hook.rs."}}
{"type":"assistant","sessionId":"cifixture01","uuid":"u2","message":{"role":"assistant","content":[{"type":"tool_use","id":"cf2","name":"Read","input":{"file_path":"/repo/src/session/hook.rs"}}]}}
{"type":"assistant","sessionId":"cifixture01","uuid":"u3","message":{"role":"assistant","content":[{"type":"tool_use","id":"cf3","name":"Bash","input":{"command":"git status"}}]}}
"#;

// --- decision model ----------------------------------------------------------

/// One prior tool call, summarised for judge context.
#[derive(Debug, Clone, Serialize)]
struct PriorCall {
    tool: String,
    summary: String,
}

/// A sampled decision: an assistant turn that chose a non-lens, non-ToolSearch
/// primitive.
#[derive(Debug, Clone, Serialize)]
struct Decision {
    /// Stable, globally-unique id (the tool_use id) used for ordering + dedupe.
    turn_id: String,
    /// 8-char session id (mined_cases shape).
    session: String,
    /// Per-session tool-call index (mined_cases `ts`).
    ts: u64,
    /// Most recent genuine user text before this turn.
    user_text: String,
    /// Up to 3 prior tool calls (any tool), oldest first.
    prior: Vec<PriorCall>,
    /// The chosen primitive (Read / Grep / Bash / Edit / ...).
    chosen_tool: String,
    /// Raw tool input (used by the judge + for file extraction).
    chosen_input: Value,
    /// File basenames referenced by the chosen input (mined_cases `files`).
    files: Vec<String>,
}

/// A judged decision, persisted in the checkpoint. Carries just enough to
/// re-aggregate without re-reading transcripts.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JudgedRec {
    chosen: String,
    verdict: String,
    session: String,
    ts: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct Counts {
    num: u64,
    den: u64,
}

/// The on-disk checkpoint / report. `judged` is the source of truth; the
/// `miss_rate` / `counts` / `verdicts` fields are recomputed each run and kept
/// in the file for readability. `#[serde(default)]` keeps old/partial files
/// loadable.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Checkpoint {
    #[serde(default)]
    mock: bool,
    #[serde(default)]
    n_judged_total: usize,
    #[serde(default)]
    miss_rate: BTreeMap<String, f64>,
    #[serde(default)]
    counts: BTreeMap<String, Counts>,
    #[serde(default)]
    verdicts: BTreeMap<String, u64>,
    #[serde(default)]
    judged: BTreeMap<String, JudgedRec>,
}

// --- transcript parsing ------------------------------------------------------

/// Extract genuine user text from a `user` message's `content`: a raw string, or
/// the concatenated `text` blocks of a content array. Tool-result-only turns
/// (no text block) return `None` so they never clobber the real user intent.
fn user_text_of(content: &Value) -> Option<String> {
    match content {
        Value::String(s) => {
            let s = s.trim();
            (!s.is_empty()).then(|| s.to_string())
        }
        Value::Array(items) => {
            let text: String = items
                .iter()
                .filter(|it| it.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|it| it.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        _ => None,
    }
}

/// basename of a slash path, ignoring any escaped spaces the transcript records.
fn basename(path: &str) -> String {
    path.replace("\\ ", " ")
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim()
        .to_string()
}

/// Files referenced by a tool input, for the mined-case `files` field.
fn files_of(tool: &str, input: &Value) -> Vec<String> {
    let field = match tool {
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => "file_path",
        "Grep" | "Glob" => "path",
        _ => return Vec::new(),
    };
    input
        .get(field)
        .and_then(Value::as_str)
        .map(|p| vec![basename(p)])
        .unwrap_or_default()
}

/// A compact one-line summary of a tool call for prior-context + evidence.
fn input_summary(tool: &str, input: &Value) -> String {
    let pick = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" => {
            format!("file_path={}", basename(pick("file_path")))
        }
        "Grep" => format!("pattern={:?}", pick("pattern")),
        "Glob" => format!("glob={:?}", pick("pattern")),
        "Bash" => {
            let c = pick("command");
            let c: String = c.chars().take(80).collect();
            format!("command={c:?}")
        }
        _ => {
            let s = serde_json::to_string(input).unwrap_or_default();
            s.chars().take(80).collect()
        }
    }
}

/// A tool call that is neither a lens MCP tool nor the lens `ToolSearch`
/// bootstrap is a "decision" — a place the agent picked a primitive.
fn is_decision_tool(name: &str) -> bool {
    !name.starts_with("mcp__lens__") && name != "ToolSearch"
}

/// Sample the decisions from one session's transcript lines, in transcript
/// order. `fallback_session` is used when a line omits `sessionId`.
///
/// Deterministic: no RNG, no clock; output is a pure function of the input.
fn sample_session(lines: &[&str], fallback_session: &str) -> Vec<Decision> {
    let mut decisions = Vec::new();
    let mut last_user = String::new();
    let mut recent: VecDeque<PriorCall> = VecDeque::with_capacity(3);
    let mut ts: u64 = 0;

    for raw in lines {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        let kind = obj.get("type").and_then(Value::as_str).unwrap_or("");

        if kind == "user" {
            if let Some(c) = obj.pointer("/message/content") {
                if let Some(text) = user_text_of(c) {
                    last_user = text;
                }
            }
            continue;
        }
        if kind != "assistant" {
            continue;
        }
        let Some(content) = obj.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        let session = obj
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or(fallback_session);
        let session8: String = session.chars().take(8).collect();
        let uuid = obj.get("uuid").and_then(Value::as_str).unwrap_or("");

        for (idx, item) in content.iter().enumerate() {
            if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = item.get("name").and_then(Value::as_str) else {
                continue;
            };
            let input = item.get("input").cloned().unwrap_or(Value::Null);
            ts += 1;

            if is_decision_tool(name) {
                let turn_id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{session8}:{uuid}#{idx}"));
                decisions.push(Decision {
                    turn_id,
                    session: session8.clone(),
                    ts,
                    user_text: last_user.clone(),
                    prior: recent.iter().cloned().collect(),
                    chosen_tool: name.to_string(),
                    chosen_input: input.clone(),
                    files: files_of(name, &input),
                });
            }

            // Every tool call (lens included) is prior-context for later turns.
            if recent.len() == 3 {
                recent.pop_front();
            }
            recent.push_back(PriorCall {
                tool: name.to_string(),
                summary: input_summary(name, &input),
            });
        }
    }
    decisions
}

/// Sample decisions across every `*.jsonl` in `dir` (sorted for determinism).
/// Falls back to the embedded fixture when the dir is missing/empty so `--mock`
/// always yields output.
fn sample_all(dir: &Path) -> Vec<Decision> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().map(|e| e == "jsonl").unwrap_or(false))
                .collect()
        })
        .unwrap_or_default();
    files.sort();

    if files.is_empty() {
        eprintln!("no transcripts under {} — using embedded fixture", dir.display());
        let lines: Vec<&str> = EMBEDDED_FIXTURE.lines().collect();
        return sample_session(&lines, "cifixture01");
    }

    let mut all = Vec::new();
    for f in &files {
        let Ok(body) = std::fs::read_to_string(f) else {
            continue;
        };
        let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("session");
        let lines: Vec<&str> = body.lines().collect();
        all.extend(sample_session(&lines, stem));
    }
    all
}

// --- mock judge (deterministic) ----------------------------------------------

fn re_bare_ident() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").unwrap())
}

fn re_def_kw() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| {
        regex::Regex::new(r"\b(fn|func|def|class|struct|enum|type|const|interface|function)\s+\w+")
            .unwrap()
    })
}

const IDENT_STOPWORDS: [&str; 12] = [
    "the", "and", "for", "error", "message", "timeout", "todo", "test", "value", "result", "data",
    "self",
];

const CODE_EXTS: [&str; 24] = [
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "swift", "java", "c", "cc", "cpp",
    "h", "hpp", "rb", "kt", "kts", "scala", "php", "lua", "sh", "cs",
];

/// A multi-token STRUCTURAL grep that grep_ast answers without false positives.
/// Deliberately excludes lone `fn`/`struct`/`enum` def patterns — those route to
/// lens_symbol below (the T1 vs T5 split).
fn is_syntax_shape(pattern: &str) -> bool {
    let p = pattern.to_ascii_lowercase();
    ["impl ", "#[", "async fn", "->", "trait "]
        .iter()
        .any(|needle| p.contains(needle))
}

fn mentions_any(hay: &str, needles: &[&str]) -> bool {
    let h = hay.to_ascii_lowercase();
    needles.iter().any(|n| h.contains(n))
}

fn is_links_ctx(user_text: &str) -> bool {
    mentions_any(
        user_text,
        &[
            "who calls",
            "what calls",
            "callers",
            "call site",
            "used by",
            "who uses",
            "references to",
            "where is it used",
            "where it's used",
        ],
    )
}

fn is_path_ctx(user_text: &str) -> bool {
    let u = user_text.to_ascii_lowercase();
    (u.contains("how does") && u.contains("reach"))
        || u.contains("path from")
        || u.contains("path between")
        || u.contains("flow from")
        || (u.contains("trace") && u.contains("reach"))
}

fn has_code_ext(path: &str) -> bool {
    let p = basename(path).to_ascii_lowercase();
    p.rsplit_once('.')
        .map(|(_, ext)| CODE_EXTS.contains(&ext))
        .unwrap_or(false)
}

/// Deterministic stand-in for the model judge: maps (chosen tool, input,
/// context) to a verdict with fixed rules, so `--mock` is fully reproducible and
/// spends zero quota.
fn mock_verdict(d: &Decision) -> String {
    let inp = &d.chosen_input;
    let s = |k: &str| inp.get(k).and_then(Value::as_str).unwrap_or("");
    match d.chosen_tool.as_str() {
        "Grep" => {
            let pattern = s("pattern");
            let bare_ident = re_bare_ident().is_match(pattern)
                && pattern.len() >= 3
                && !IDENT_STOPWORDS.contains(&pattern.to_ascii_lowercase().as_str());
            if is_syntax_shape(pattern) {
                "lens_grep_ast"
            } else if is_links_ctx(&d.user_text) {
                "lens_links"
            } else if is_path_ctx(&d.user_text) {
                "lens_path"
            } else if bare_ident || re_def_kw().is_match(pattern) {
                "lens_symbol"
            } else {
                "none"
            }
        }
        "Read" => {
            let path = s("file_path");
            let whole = inp.get("offset").is_none() && inp.get("limit").is_none();
            if has_code_ext(path) && whole {
                let prior_reads = d.prior.iter().filter(|c| c.tool == "Read").count();
                if prior_reads >= 2 {
                    "lens_symbol" // consecutive whole-file reads = hand-tracing (nav-run)
                } else {
                    "lens_skeleton"
                }
            } else {
                "none"
            }
        }
        // Bash / Edit / Write / Glob / ... : no graph/structure equivalent here.
        _ => "none",
    }
    .to_string()
}

// --- live judge --------------------------------------------------------------

fn render_prompt(template: &str, d: &Decision) -> String {
    let prior = if d.prior.is_empty() {
        "(none)".to_string()
    } else {
        d.prior
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {} {}", i + 1, c.tool, c.summary))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let user = if d.user_text.is_empty() {
        "(no user text captured)".to_string()
    } else {
        d.user_text.clone()
    };
    let chosen_input = serde_json::to_string_pretty(&d.chosen_input).unwrap_or_default();
    template
        .replace("{{USER_TEXT}}", &user)
        .replace("{{PRIOR_CALLS}}", &prior)
        .replace("{{CHOSEN_TOOL}}", &d.chosen_tool)
        .replace("{{CHOSEN_INPUT}}", &chosen_input)
}

/// Parse a verdict token out of the judge's raw text. Scans tokens from the end
/// (the model's final answer) for the first recognised verdict; defaults to
/// `none` if nothing matches.
fn parse_verdict(raw: &str) -> String {
    for tok in raw.split_whitespace().rev() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
        if KNOWN_VERDICTS.contains(&t) {
            return t.to_string();
        }
    }
    "none".to_string()
}

fn which_on_path(bin: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|dir| Path::new(dir).join(bin).exists())
}

/// One live judge call. Prefers `claude-pty` (plan quota) when on PATH, else
/// falls back to `claude -p` bounded by `perl alarm` (headless claude has no
/// timeout flag). Tools are disabled — this is a pure classification.
fn judge_live(prompt: &str) -> Result<String, String> {
    let out = if which_on_path("claude-pty") {
        let repo = env!("CARGO_MANIFEST_DIR");
        let mut child = Command::new("claude-pty")
            .args(["--working-dir", repo, "--allowed-tools", "", "--timeout", "120"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("spawning claude-pty: {e}"))?;
        child
            .stdin
            .as_mut()
            .ok_or("claude-pty stdin unavailable")?
            .write_all(prompt.as_bytes())
            .map_err(|e| format!("writing claude-pty stdin: {e}"))?;
        child
            .wait_with_output()
            .map_err(|e| format!("waiting on claude-pty: {e}"))?
    } else {
        let mut cmd = Command::new("perl");
        cmd.args(["-e", "alarm shift; exec @ARGV", "120", "claude", "-p", prompt])
            .args(["--allowedTools", ""])
            .args(["--output-format", "text"]);
        if let Ok(m) = std::env::var("LENS_JUDGE_MODEL") {
            if !m.is_empty() {
                cmd.args(["--model", m.as_str()]);
            }
        }
        if let Ok(e) = std::env::var("LENS_JUDGE_EFFORT") {
            if !e.is_empty() {
                cmd.args(["--effort", e.as_str()]);
            }
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("spawning claude: {e}"))?
    };
    if !out.status.success() {
        return Err(format!(
            "judge exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(300)
                .collect::<String>()
        ));
    }
    Ok(parse_verdict(&String::from_utf8_lossy(&out.stdout)))
}

// --- aggregation -------------------------------------------------------------

/// Recompute the per-tool miss-rate, raw counts, and verdict tally from the full
/// judged set. Always emits all five `AGG_TOOLS` keys (0.0 when den == 0).
fn aggregate(
    judged: &BTreeMap<String, JudgedRec>,
) -> (BTreeMap<String, f64>, BTreeMap<String, Counts>, BTreeMap<String, u64>) {
    let mut counts: BTreeMap<String, Counts> = AGG_TOOLS
        .iter()
        .map(|t| (t.to_string(), Counts::default()))
        .collect();
    let mut verdicts: BTreeMap<String, u64> = BTreeMap::new();

    for rec in judged.values() {
        *verdicts.entry(rec.verdict.clone()).or_default() += 1;
        for tool in AGG_TOOLS {
            if addressable(tool).contains(&rec.chosen.as_str()) {
                let c = counts.get_mut(tool).expect("agg tool preseeded");
                c.den += 1;
                if rec.verdict == tool {
                    c.num += 1;
                }
            }
        }
    }

    let miss_rate = counts
        .iter()
        .map(|(t, c)| {
            let r = if c.den == 0 {
                0.0
            } else {
                c.num as f64 / c.den as f64
            };
            (t.clone(), r)
        })
        .collect();
    (miss_rate, counts, verdicts)
}

/// A flagged decision rendered in the committed `cases/mined_cases.json` shape.
fn mined_case(d: &Decision, verdict: &str) -> Value {
    let pattern = format!(
        "{}-{}",
        d.chosen_tool.to_ascii_lowercase(),
        verdict.strip_prefix("lens_").unwrap_or(verdict)
    );
    let evidence = {
        let prior: Vec<String> = d.prior.iter().map(|c| format!("{} {}", c.tool, c.summary)).collect();
        let mut ev = prior;
        ev.push(format!("{} {}", d.chosen_tool, input_summary(&d.chosen_tool, &d.chosen_input)));
        ev.join("; ")
    };
    let intent = format!(
        "Chose {} ({}); a lens judge found {} the better fit for this decision.",
        d.chosen_tool,
        input_summary(&d.chosen_tool, &d.chosen_input),
        verdict
    );
    serde_json::json!({
        "session": d.session,
        "ts": d.ts,
        "pattern": pattern,
        "files": d.files,
        "intent": intent,
        "right_tool": verdict,
        "evidence": evidence,
    })
}

// --- checkpoint IO -----------------------------------------------------------

fn load_checkpoint(path: &Path) -> Checkpoint {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn build_checkpoint(judged: BTreeMap<String, JudgedRec>, mock: bool) -> Checkpoint {
    let (miss_rate, counts, verdicts) = aggregate(&judged);
    Checkpoint {
        mock,
        n_judged_total: judged.len(),
        miss_rate,
        counts,
        verdicts,
        judged,
    }
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(value)?)?;
    Ok(())
}

// --- main --------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mock = args.iter().any(|a| a == "--mock");
    let n = args
        .iter()
        .position(|a| a == "--n")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(40);

    let out_dir = results_dir();
    let miss_path = out_dir.join("intent_miss.json");
    let mined_path = out_dir.join("intent_mined_cases.json");

    // Resume: load prior verdicts, skip their turn-ids.
    let mut checkpoint = load_checkpoint(&miss_path);
    let already: std::collections::HashSet<String> = checkpoint.judged.keys().cloned().collect();

    // Deterministic candidate order: sort every sampled decision by turn-id.
    let mut all = sample_all(&transcript_dir());
    all.sort_by(|a, b| a.turn_id.cmp(&b.turn_id));
    let by_turn: BTreeMap<String, Decision> =
        all.iter().map(|d| (d.turn_id.clone(), d.clone())).collect();

    let to_judge: Vec<&Decision> = all
        .iter()
        .filter(|d| !already.contains(&d.turn_id))
        .take(n)
        .collect();

    eprintln!(
        "sampled {} decisions ({} already judged); judging {} new this run{}",
        all.len(),
        already.len(),
        to_judge.len(),
        if mock { " (mock)" } else { " (live judge)" }
    );

    let template = if mock {
        String::new()
    } else {
        std::fs::read_to_string(judge_prompt_path())
            .map_err(|e| anyhow::anyhow!("reading judge_prompt.txt: {e}"))?
    };

    let mut judged = std::mem::take(&mut checkpoint.judged);
    for d in &to_judge {
        let verdict = if mock {
            mock_verdict(d)
        } else {
            match judge_live(&render_prompt(&template, d)) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("  judge failed for {}: {e} (will retry next run)", d.turn_id);
                    continue;
                }
            }
        };
        judged.insert(
            d.turn_id.clone(),
            JudgedRec {
                chosen: d.chosen_tool.clone(),
                verdict,
                session: d.session.clone(),
                ts: d.ts,
            },
        );
        // Persist after each *costly* live judge so an interrupted run resumes.
        if !mock {
            write_json(&miss_path, &build_checkpoint(judged.clone(), mock))?;
        }
    }

    let final_cp = build_checkpoint(judged, mock);
    write_json(&miss_path, &final_cp)?;

    // Emit mined precedents for every flagged (verdict ∈ AGG_TOOLS) judged
    // decision we still have full context for, in `mined_cases.json` shape.
    let mut mined: Vec<Value> = final_cp
        .judged
        .iter()
        .filter(|(_, rec)| AGG_TOOLS.contains(&rec.verdict.as_str()))
        .filter_map(|(tid, rec)| by_turn.get(tid).map(|d| mined_case(d, &rec.verdict)))
        .collect();
    mined.sort_by(|a, b| {
        let key = |v: &Value| {
            (
                v.get("session").and_then(Value::as_str).unwrap_or("").to_string(),
                v.get("ts").and_then(Value::as_u64).unwrap_or(0),
            )
        };
        key(a).cmp(&key(b))
    });
    write_json(&mined_path, &mined)?;

    println!("wrote {}", miss_path.display());
    println!("wrote {} ({} mined cases)", mined_path.display(), mined.len());
    println!(
        "{}",
        serde_json::to_string_pretty(&final_cp.miss_rate)?
    );
    Ok(())
}

// --- tests -------------------------------------------------------------------
// Hermetic: only the pure sample/judge/aggregate fns run. `judge_live` / `main`
// (the only fns that spawn a subprocess or touch the real filesystem) are never
// called here, so `cargo test` makes zero subprocess/network calls.

#[cfg(test)]
mod tests {
    use super::*;

    /// One crafted session exercising every mock verdict branch. 1 tool-call per
    /// assistant turn (matches the real transcript shape). Includes a lens call
    /// and a ToolSearch (must be skipped) and a tool-result user turn (must not
    /// clobber the captured user text).
    const FIXTURE: &str = r#"{"type":"system","subtype":"init","sessionId":"unittest0000"}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":"search for the tool implementations"}}
{"type":"assistant","sessionId":"unittest0000","uuid":"a","message":{"role":"assistant","content":[{"type":"tool_use","id":"A","name":"Grep","input":{"pattern":"impl Forge"}}]}}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":"where is TcpListener defined"}}
{"type":"assistant","sessionId":"unittest0000","uuid":"b","message":{"role":"assistant","content":[{"type":"tool_use","id":"B","name":"Grep","input":{"pattern":"TcpListener"}}]}}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":"who calls fired in the throttle module"}}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"B","content":"..."}]}}
{"type":"assistant","sessionId":"unittest0000","uuid":"c","message":{"role":"assistant","content":[{"type":"tool_use","id":"C","name":"Grep","input":{"pattern":"fired"}}]}}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":"how does route_inner reach to_hook_json"}}
{"type":"assistant","sessionId":"unittest0000","uuid":"d","message":{"role":"assistant","content":[{"type":"tool_use","id":"D","name":"Grep","input":{"pattern":"to_hook_json"}}]}}
{"type":"user","sessionId":"unittest0000","message":{"role":"user","content":"understand the server module"}}
{"type":"assistant","sessionId":"unittest0000","uuid":"e","message":{"role":"assistant","content":[{"type":"tool_use","id":"E","name":"Read","input":{"file_path":"/x/src/server.rs"}}]}}
{"type":"assistant","sessionId":"unittest0000","uuid":"g","message":{"role":"assistant","content":[{"type":"tool_use","id":"G","name":"Read","input":{"file_path":"/x/README.md"}}]}}
{"type":"assistant","sessionId":"unittest0000","uuid":"h","message":{"role":"assistant","content":[{"type":"tool_use","id":"H","name":"Bash","input":{"command":"git status"}}]}}
{"type":"assistant","sessionId":"unittest0000","uuid":"l","message":{"role":"assistant","content":[{"type":"tool_use","id":"L","name":"mcp__lens__lens_search","input":{"queries":["x"]}}]}}
{"type":"assistant","sessionId":"unittest0000","uuid":"t","message":{"role":"assistant","content":[{"type":"tool_use","id":"TS","name":"ToolSearch","input":{"query":"select:lens_symbol"}}]}}"#;

    fn judged_from_fixture() -> BTreeMap<String, JudgedRec> {
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "unittest0000");
        decisions
            .iter()
            .map(|d| {
                (
                    d.turn_id.clone(),
                    JudgedRec {
                        chosen: d.chosen_tool.clone(),
                        verdict: mock_verdict(d),
                        session: d.session.clone(),
                        ts: d.ts,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn sampling_excludes_lens_and_toolsearch_calls() {
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "unittest0000");
        // A,B,C,D (Grep) + E,G (Read) + H (Bash) = 7 ; L (lens) and TS excluded.
        assert_eq!(decisions.len(), 7);
        let ids: Vec<&str> = decisions.iter().map(|d| d.turn_id.as_str()).collect();
        assert!(!ids.contains(&"L"), "lens call must not be a decision");
        assert!(!ids.contains(&"TS"), "ToolSearch must not be a decision");
    }

    #[test]
    fn tool_result_user_turn_does_not_clobber_intent() {
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "unittest0000");
        let c = decisions.iter().find(|d| d.turn_id == "C").unwrap();
        // The tool_result user turn between the prompt and C must be skipped, so
        // C still sees the genuine "who calls fired" intent -> lens_links.
        assert!(c.user_text.contains("who calls fired"));
        assert_eq!(mock_verdict(c), "lens_links");
    }

    #[test]
    fn mock_verdicts_hit_each_branch() {
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "unittest0000");
        let v = |id: &str| {
            let d = decisions.iter().find(|d| d.turn_id == id).unwrap();
            mock_verdict(d)
        };
        assert_eq!(v("A"), "lens_grep_ast"); // "impl Forge"
        assert_eq!(v("B"), "lens_symbol"); // bare ident "TcpListener"
        assert_eq!(v("C"), "lens_links"); // "who calls fired"
        assert_eq!(v("D"), "lens_path"); // "how does ... reach ..."
        assert_eq!(v("E"), "lens_skeleton"); // whole server.rs read
        assert_eq!(v("G"), "none"); // README.md read
        assert_eq!(v("H"), "none"); // bash
    }

    #[test]
    fn aggregation_matches_hand_computed_miss_rates() {
        let judged = judged_from_fixture();
        let (miss, counts, _verdicts) = aggregate(&judged);

        // Denominators: Grep decisions = {A,B,C,D} (4), Read = {E,G} (2).
        // addressable(symbol/links/path)={Grep,Read} -> den 6 each.
        // addressable(grep_ast)={Grep} -> den 4. addressable(skeleton)={Read} -> den 2.
        // Numerators: symbol<-B, links<-C, path<-D, grep_ast<-A, skeleton<-E.
        let approx = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(approx(miss["lens_symbol"], 1.0 / 6.0), "{miss:?}");
        assert!(approx(miss["lens_links"], 1.0 / 6.0), "{miss:?}");
        assert!(approx(miss["lens_path"], 1.0 / 6.0), "{miss:?}");
        assert!(approx(miss["lens_grep_ast"], 1.0 / 4.0), "{miss:?}");
        assert!(approx(miss["lens_skeleton"], 1.0 / 2.0), "{miss:?}");

        assert_eq!(counts["lens_symbol"], Counts { num: 1, den: 6 });
        assert_eq!(counts["lens_grep_ast"], Counts { num: 1, den: 4 });
        assert_eq!(counts["lens_skeleton"], Counts { num: 1, den: 2 });

        // All five aggregation keys are always present.
        for t in AGG_TOOLS {
            assert!(miss.contains_key(t), "missing miss-rate key {t}");
        }
    }

    #[test]
    fn parse_verdict_takes_the_final_recognised_token() {
        assert_eq!(parse_verdict("lens_symbol\n"), "lens_symbol");
        assert_eq!(parse_verdict("The better fit is lens_skeleton."), "lens_skeleton");
        assert_eq!(parse_verdict("reason-instead-of-query"), "reason-instead-of-query");
        assert_eq!(parse_verdict("no idea at all"), "none");
    }

    #[test]
    fn resume_skips_judged_and_extends_counts() {
        // First batch of 1: only A judged.
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let mut all = sample_session(&lines, "unittest0000");
        all.sort_by(|a, b| a.turn_id.cmp(&b.turn_id));
        let mut judged: BTreeMap<String, JudgedRec> = BTreeMap::new();
        let first = &all[0];
        judged.insert(
            first.turn_id.clone(),
            JudgedRec {
                chosen: first.chosen_tool.clone(),
                verdict: mock_verdict(first),
                session: first.session.clone(),
                ts: first.ts,
            },
        );
        let n1 = judged.len();
        // Resume: judge the rest, skipping the already-judged turn-id.
        let remaining: Vec<Decision> = all
            .iter()
            .filter(|d| !judged.contains_key(&d.turn_id))
            .cloned()
            .collect();
        for d in &remaining {
            judged.insert(
                d.turn_id.clone(),
                JudgedRec {
                    chosen: d.chosen_tool.clone(),
                    verdict: mock_verdict(d),
                    session: d.session.clone(),
                    ts: d.ts,
                },
            );
        }
        assert_eq!(n1, 1);
        assert_eq!(judged.len(), 7, "resume must reach the full judged set exactly once");
    }

    #[test]
    fn mined_case_matches_committed_shape() {
        let lines: Vec<&str> = FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "unittest0000");
        let b = decisions.iter().find(|d| d.turn_id == "B").unwrap();
        let case = mined_case(b, "lens_symbol");
        for key in ["session", "ts", "pattern", "files", "intent", "right_tool", "evidence"] {
            assert!(case.get(key).is_some(), "mined case missing {key}");
        }
        assert_eq!(case["right_tool"], "lens_symbol");
        assert_eq!(case["session"], "unittest");
    }

    #[test]
    fn embedded_fixture_yields_decisions_for_ci() {
        let lines: Vec<&str> = EMBEDDED_FIXTURE.lines().collect();
        let decisions = sample_session(&lines, "cifixture01");
        // Grep(fired) + Read(hook.rs) + Bash(git status) = 3 decisions.
        assert_eq!(decisions.len(), 3);
        let judged: BTreeMap<String, JudgedRec> = decisions
            .iter()
            .map(|d| {
                (
                    d.turn_id.clone(),
                    JudgedRec {
                        chosen: d.chosen_tool.clone(),
                        verdict: mock_verdict(d),
                        session: d.session.clone(),
                        ts: d.ts,
                    },
                )
            })
            .collect();
        let (miss, _c, _v) = aggregate(&judged);
        for t in AGG_TOOLS {
            assert!(miss.contains_key(t));
        }
    }
}
