//! Tool-selection eval: does the model engage lens tools before drifting into
//! the Grep→Read→Read chain when asked a read-only structural question about
//! this repo?
//!
//! Primary metric (`rate`): a run passes iff a lens tool appears within the
//! first 3 file-inspection calls (Read/Grep/Glob/Bash or any `mcp__lens__*`).
//! This scores the chain, not just the opening pick — a Grep→lens recovery
//! passes, a Grep→Read→Read flood fails — because the product goal is context
//! protection, and first-pick-only scoring counted a full lens_search
//! conversion as 0.00 (measured, task 0008 v3).
//! Secondary metrics per run: `strict` (first inspection call is in the
//! task's `expected_tools` — the original bar, kept for attribution) and
//! `lens_first` (first inspection call is any lens tool).
//! Unlike `bench_accuracy`, this spawns the real `claude -p` CLI with the real
//! lens MCP server live (via a temp `--mcp-config`), so it measures actual
//! tool-selection behavior, not a context-presence oracle.
//!
//!   cargo run --release --bin bench_toolsel -- --dry-run   # validate tasks, spawn nothing
//!   target/release/bench_toolsel --runs 3                  # live: needs `claude` on PATH
//!                                                           # and target/release/lens built

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

// --- Task spec ---------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Task {
    id: String,
    prompt: String,
    #[serde(default)]
    #[allow(dead_code)] // not yet consumed; reserved for tasks needing a scratch repo
    repo_fixture: Option<String>,
    expected_tools: Vec<String>,
    score: String,
    source: String,
}

fn tasks_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benchmarks/toolsel/tasks")
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Load and sort all task specs from `dir`, rejecting malformed JSON up front
/// so a typo'd task fails loudly instead of silently vanishing.
fn load_tasks(dir: &Path) -> anyhow::Result<Vec<Task>> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
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
        validate_task(&task).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
        tasks.push(task);
    }
    Ok(tasks)
}

/// Schema checks shared by the loader and `--dry-run`.
fn validate_task(task: &Task) -> anyhow::Result<()> {
    if task.id.is_empty() {
        anyhow::bail!("empty id");
    }
    if task.prompt.is_empty() {
        anyhow::bail!("empty prompt");
    }
    if task.expected_tools.is_empty() {
        anyhow::bail!("expected_tools must not be empty");
    }
    if task.score != "first_tool" {
        anyhow::bail!("unsupported score kind: {}", task.score);
    }
    Ok(())
}

/// `--dry-run` table rows: `id | expected_tools | source`, one per task.
fn dry_run_lines(tasks: &[Task]) -> Vec<String> {
    tasks
        .iter()
        .map(|t| format!("{} | {} | {}", t.id, t.expected_tools.join(","), t.source))
        .collect()
}

fn print_dry_run_table(tasks: &[Task]) {
    println!("id | expected_tools | source");
    for line in dry_run_lines(tasks) {
        println!("{line}");
    }
    println!("{} task(s) validated", tasks.len());
}

// --- stream-json tool_use parsing --------------------------------------------

/// Normalize a raw tool name: `mcp__lens__lens_X` -> `lens_X`; everything else
/// (Read, Grep, Glob, Bash, ...) is unchanged.
fn normalize_tool(name: &str) -> String {
    name.strip_prefix("mcp__lens__").unwrap_or(name).to_string()
}

/// Tools counted when picking the first file-inspection call: builtin
/// Read/Grep/Glob/Bash, or any lens MCP tool. Other tool calls (TodoWrite,
/// WebSearch, ...) are skipped over rather than ending the scan.
fn is_inspection_tool(raw_name: &str) -> bool {
    matches!(raw_name, "Read" | "Grep" | "Glob" | "Bash") || raw_name.starts_with("mcp__lens__")
}

/// Parse a `claude -p --output-format stream-json` transcript (one JSON object
/// per line) and return every `tool_use` name in call order, raw
/// (un-normalized).
fn parse_tool_use_sequence(stream: &str) -> Vec<String> {
    let mut tools = Vec::new();
    for line in stream.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(obj) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(content) = obj.pointer("/message/content").and_then(Value::as_array) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let Some(name) = item.get("name").and_then(Value::as_str) {
                    tools.push(name.to_string());
                }
            }
        }
    }
    tools
}

/// `first_tool` scoring: the full tool sequence normalized for the record, plus
/// pass/fail on whether the first inspection-tool call is in `expected`. No
/// inspection tool called at all -> fail.
fn score_first_tool(raw_tools: &[String], expected: &[String]) -> (Vec<String>, bool) {
    let normalized: Vec<String> = raw_tools.iter().map(|n| normalize_tool(n)).collect();
    let pass = raw_tools
        .iter()
        .find(|n| is_inspection_tool(n))
        .is_some_and(|raw| expected.contains(&normalize_tool(raw)));
    (normalized, pass)
}

/// Secondary metric: the first inspection tool is ANY lens tool, whether or not
/// it's in the task's expected set. A run that opens with lens_search on a
/// graph-nav task fails the strict metric but is still a lens win (the bytes
/// stayed contained); this keeps that visible without loosening the pass bar.
fn first_is_lens(raw_tools: &[String]) -> bool {
    raw_tools
        .iter()
        .find(|n| is_inspection_tool(n))
        .is_some_and(|raw| raw.starts_with("mcp__lens__"))
}

/// How many inspection calls a run gets to engage lens before the chain metric
/// fails it: 3 is exactly the measured drift signature (Grep,Read,Read).
const CHAIN_WINDOW: usize = 3;

/// Primary (chain) metric: a lens tool appears within the first
/// [`CHAIN_WINDOW`] inspection calls. Credits a Grep→lens recovery, fails the
/// Grep→Read→Read flood.
fn lens_within_window(raw_tools: &[String]) -> bool {
    raw_tools
        .iter()
        .filter(|n| is_inspection_tool(n))
        .take(CHAIN_WINDOW)
        .any(|raw| raw.starts_with("mcp__lens__"))
}

// --- Live invocation (never reached from tests) ------------------------------

/// MCP config pointing at the release lens binary with no subcommand — the
/// same server semantics `lens setup`'s `register_mcp` registers
/// (`src/setup.rs`): the bare binary, no args, since `lens` with no subcommand
/// is the MCP stdio server (`src/main.rs`).
fn mcp_config_json(lens_bin: &Path) -> Value {
    serde_json::json!({
        "mcpServers": {
            "lens": {
                "command": lens_bin.to_string_lossy(),
                "args": []
            }
        }
    })
}

fn lens_release_bin() -> PathBuf {
    repo_root().join("target/release/lens")
}

/// Whether the installed `claude` CLI recognizes `--strict-mcp-config`
/// (checked once per process; older CLIs lack it).
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

/// One `claude -p` call for `task`, with the lens MCP server live via a temp
/// `--mcp-config`. `cwd` is the repo root: the lens MCP server pins its cwd at
/// spawn, so this is what makes it index this repo's tree. Wall-clock bounded
/// by `perl alarm` (mirrors `benchmarks/common/accuracy.rs` hygiene; headless
/// `claude` has no built-in timeout flag).
fn invoke_claude(task: &Task, mcp_config: &Path) -> Result<String, String> {
    let workdir = repo_root();
    let mut cmd = Command::new("perl");
    cmd.current_dir(&workdir)
        .args(["-e", "alarm shift; exec @ARGV", "180"])
        .arg("claude")
        .arg("-p")
        .arg(&task.prompt)
        .arg("--mcp-config")
        .arg(mcp_config);
    if supports_strict_mcp_config() {
        cmd.arg("--strict-mcp-config");
    }
    // Optional: wire lens's own hooks (SessionStart repo-map digest, PreToolUse
    // steering/deny, PostToolUse reset) into the headless session so the live
    // measurement exercises the hook-based steering, not just the MCP tool
    // descriptions. The installed hooks point at the released binary; set
    // LENS_TOOLSEL_SETTINGS to a settings JSON that points them at the build
    // under test. Unset -> descriptions-only (the ablation baseline).
    if let Ok(settings) = std::env::var("LENS_TOOLSEL_SETTINGS") {
        if !settings.is_empty() {
            cmd.args(["--settings", settings.as_str()]);
        }
    }
    cmd.args(["--allowedTools", "Read Grep Glob Bash mcp__lens"])
        .args(["--output-format", "stream-json"])
        .arg("--verbose")
        .args(["--max-turns", "8"]);

    let out = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawning claude: {e}"))?
        .wait_with_output()
        .map_err(|e| format!("waiting on claude: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "claude exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
                .chars()
                .take(500)
                .collect::<String>()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// One task run: write the temp MCP config, invoke `claude -p`, parse the
/// ordered tool_use names. Retry-once on failure (a transient kill/timeout
/// succeeds on the second attempt), matching `accuracy.rs`'s hygiene.
fn run_task_once(task: &Task, lens_bin: &Path) -> anyhow::Result<Vec<String>> {
    let mut cfg_file = tempfile::NamedTempFile::new()?;
    cfg_file.write_all(serde_json::to_string(&mcp_config_json(lens_bin))?.as_bytes())?;
    let cfg_path = cfg_file.path().to_path_buf();

    let mut last_err = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
        match invoke_claude(task, &cfg_path) {
            Ok(stream) => return Ok(parse_tool_use_sequence(&stream)),
            Err(e) => {
                eprintln!("  task {} attempt {} failed: {e}", task.id, attempt + 1);
                last_err = e;
            }
        }
    }
    anyhow::bail!("task {}: {last_err}", task.id)
}

#[derive(Debug, Serialize)]
struct RunResult {
    tools: Vec<String>,
    /// Chain metric: lens engaged within the first CHAIN_WINDOW inspection calls.
    pass: bool,
    /// Original bar: first inspection call is in the task's expected_tools.
    strict: bool,
    lens_first: bool,
}

#[derive(Debug, Serialize)]
struct TaskResult {
    id: String,
    runs: Vec<RunResult>,
    rate: f64,
    strict_rate: f64,
    lens_first_rate: f64,
}

fn mean<'a, I: Iterator<Item = &'a TaskResult>, F: Fn(&TaskResult) -> f64>(
    results: I,
    field: F,
) -> f64 {
    let rates: Vec<f64> = results.map(field).collect();
    if rates.is_empty() {
        0.0
    } else {
        rates.iter().sum::<f64>() / rates.len() as f64
    }
}

fn run_task(task: &Task, lens_bin: &Path, runs: usize) -> TaskResult {
    let mut run_results = Vec::with_capacity(runs);
    for _ in 0..runs {
        let result = match run_task_once(task, lens_bin) {
            Ok(raw_tools) => {
                let (tools, strict) = score_first_tool(&raw_tools, &task.expected_tools);
                RunResult {
                    tools,
                    pass: lens_within_window(&raw_tools),
                    strict,
                    lens_first: first_is_lens(&raw_tools),
                }
            }
            Err(e) => {
                eprintln!("task {} run failed entirely: {e}", task.id);
                RunResult {
                    tools: Vec::new(),
                    pass: false,
                    strict: false,
                    lens_first: false,
                }
            }
        };
        run_results.push(result);
    }
    let frac = |f: fn(&RunResult) -> bool| {
        if run_results.is_empty() {
            0.0
        } else {
            run_results.iter().filter(|r| f(r)).count() as f64 / run_results.len() as f64
        }
    };
    let rate = frac(|r| r.pass);
    let strict_rate = frac(|r| r.strict);
    let lens_first_rate = frac(|r| r.lens_first);
    TaskResult {
        id: task.id.clone(),
        runs: run_results,
        rate,
        strict_rate,
        lens_first_rate,
    }
}

// --- main ---------------------------------------------------------------------

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let runs = args
        .iter()
        .position(|a| a == "--runs")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(1);

    let tasks = load_tasks(&tasks_dir())?;

    if dry_run {
        print_dry_run_table(&tasks);
        return Ok(());
    }

    let lens_bin = lens_release_bin();
    if !lens_bin.exists() {
        anyhow::bail!(
            "lens release binary not found at {} — run `cargo build --release --bin lens` first",
            lens_bin.display()
        );
    }

    let mut task_results = Vec::with_capacity(tasks.len());
    for task in &tasks {
        eprintln!("running task {} ({runs} run(s))...", task.id);
        task_results.push(run_task(task, &lens_bin, runs));
    }

    let overall_rate = mean(task_results.iter(), |r| r.rate);
    let overall_strict = mean(task_results.iter(), |r| r.strict_rate);
    let overall_lens_first = mean(task_results.iter(), |r| r.lens_first_rate);
    let mined_ids: std::collections::HashSet<&str> = tasks
        .iter()
        .filter(|t| t.source.starts_with("mined:"))
        .map(|t| t.id.as_str())
        .collect();
    let mined = || {
        task_results
            .iter()
            .filter(|r| mined_ids.contains(r.id.as_str()))
    };
    let mined_rate = mean(mined(), |r| r.rate);
    let mined_strict = mean(mined(), |r| r.strict_rate);
    let mined_lens_first = mean(mined(), |r| r.lens_first_rate);

    let model = std::env::var("LENS_BENCH_MODEL").unwrap_or_default();
    let out = serde_json::json!({
        "model": model,
        "runs": runs,
        "tasks": task_results,
        "overall_rate": overall_rate,
        "mined_rate": mined_rate,
        "overall_strict_rate": overall_strict,
        "mined_strict_rate": mined_strict,
        "overall_lens_first_rate": overall_lens_first,
        "mined_lens_first_rate": mined_lens_first,
    });

    let out_dir = repo_root().join("benchmarks/toolsel/results");
    std::fs::create_dir_all(&out_dir)?;
    let out_path = out_dir.join("toolsel.json");
    std::fs::write(&out_path, serde_json::to_string_pretty(&out)?)?;
    println!("wrote {}", out_path.display());
    println!(
        "chain: {overall_rate:.2}/{mined_rate:.2}  strict: {overall_strict:.2}/{mined_strict:.2}  lens_first: {overall_lens_first:.2}/{mined_lens_first:.2}  (overall/mined)"
    );

    Ok(())
}

// --- tests ----------------------------------------------------------------
// Hermetic: only the pure parse/load/validate/score fns above are exercised.
// `run_task_once`/`invoke_claude`/`main` (the only fns that spawn a
// subprocess) are never called from here, so `cargo test` makes zero
// subprocess/network calls.

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_TRANSCRIPT: &str = r#"{"type":"system","subtype":"init","session_id":"abc"}
{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"src/server.rs"}},{"type":"tool_use","id":"t2","name":"mcp__lens__lens_skeleton","input":{"path":"src/server.rs"}}]}}
{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"ls"}}]}}
{"type":"result","subtype":"success","result":"done"}"#;

    #[test]
    fn parses_ordered_tool_use_names_from_stream_json() {
        let tools = parse_tool_use_sequence(FIXTURE_TRANSCRIPT);
        assert_eq!(tools, vec!["Read", "mcp__lens__lens_skeleton", "Bash"]);
    }

    #[test]
    fn ignores_non_assistant_and_non_tool_use_lines() {
        let stream = "{\"type\":\"system\"}\nnot even json\n";
        assert_eq!(parse_tool_use_sequence(stream), Vec::<String>::new());
    }

    #[test]
    fn normalizes_lens_tool_names_and_leaves_others_alone() {
        assert_eq!(normalize_tool("mcp__lens__lens_skeleton"), "lens_skeleton");
        assert_eq!(normalize_tool("Read"), "Read");
    }

    #[test]
    fn first_inspection_tool_not_in_expected_fails() {
        let tools = vec!["Read".to_string(), "mcp__lens__lens_skeleton".to_string()];
        let (normalized, pass) = score_first_tool(&tools, &["lens_skeleton".to_string()]);
        assert!(!pass, "Read-first should fail even though lens_skeleton follows");
        assert_eq!(normalized, vec!["Read", "lens_skeleton"]);
    }

    #[test]
    fn first_inspection_tool_in_expected_passes() {
        let tools = vec!["mcp__lens__lens_skeleton".to_string(), "Read".to_string()];
        let (_, pass) = score_first_tool(&tools, &["lens_skeleton".to_string()]);
        assert!(pass);
    }

    #[test]
    fn non_inspection_tools_are_skipped_when_scanning_for_first() {
        // TodoWrite isn't an inspection tool; the scan should skip past it.
        let tools = vec![
            "TodoWrite".to_string(),
            "mcp__lens__lens_symbol".to_string(),
        ];
        let (_, pass) = score_first_tool(&tools, &["lens_symbol".to_string()]);
        assert!(pass);
    }

    #[test]
    fn no_inspection_tool_called_fails() {
        let tools = vec!["TodoWrite".to_string()];
        let (_, pass) = score_first_tool(&tools, &["lens_symbol".to_string()]);
        assert!(!pass);
    }

    #[test]
    fn lens_first_counts_any_lens_tool_without_loosening_the_strict_metric() {
        // lens_search first on a graph-nav task: strict metric fails,
        // secondary lens-first metric passes.
        let tools = vec![
            "TodoWrite".to_string(),
            "mcp__lens__lens_search".to_string(),
        ];
        assert!(first_is_lens(&tools));
        let (_, pass) = score_first_tool(&tools, &["lens_symbol".to_string()]);
        assert!(!pass, "strict metric must not loosen");
        // Grep first: both metrics fail.
        let grep_first = vec!["Grep".to_string(), "mcp__lens__lens_search".to_string()];
        assert!(!first_is_lens(&grep_first));
        // No inspection tool at all: lens-first is false.
        assert!(!first_is_lens(&["TodoWrite".to_string()]));
    }

    #[test]
    fn loader_rejects_malformed_task_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "{ this is not valid json").unwrap();
        assert!(load_tasks(dir.path()).is_err());
    }

    #[test]
    fn loader_accepts_well_formed_task_json() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ok.json"),
            r#"{"id":"t1","prompt":"do X","expected_tools":["lens_symbol"],"score":"first_tool","source":"seed"}"#,
        )
        .unwrap();
        let tasks = load_tasks(dir.path()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "t1");
    }

    fn sample_task(expected_tools: Vec<&str>) -> Task {
        Task {
            id: "t1".to_string(),
            prompt: "do X".to_string(),
            repo_fixture: None,
            expected_tools: expected_tools.into_iter().map(String::from).collect(),
            score: "first_tool".to_string(),
            source: "seed".to_string(),
        }
    }

    #[test]
    fn validate_rejects_empty_expected_tools() {
        assert!(validate_task(&sample_task(vec![])).is_err());
    }

    #[test]
    fn validate_accepts_well_formed_task() {
        assert!(validate_task(&sample_task(vec!["lens_symbol"])).is_ok());
    }

    #[test]
    fn dry_run_lines_include_every_task_id_and_source() {
        let tasks = vec![sample_task(vec!["lens_symbol", "lens_find"])];
        let lines = dry_run_lines(&tasks);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("t1"));
        assert!(lines[0].contains("lens_symbol,lens_find"));
        assert!(lines[0].contains("seed"));
    }

    #[test]
    fn mean_rate_of_empty_is_zero() {
        let results: Vec<TaskResult> = Vec::new();
        assert_eq!(mean(results.iter(), |r| r.rate), 0.0);
    }

    #[test]
    fn mean_rate_averages_task_rates() {
        let results = [
            TaskResult {
                id: "a".into(),
                runs: vec![],
                rate: 1.0,
                strict_rate: 0.0,
                lens_first_rate: 1.0,
            },
            TaskResult {
                id: "b".into(),
                runs: vec![],
                rate: 0.0,
                strict_rate: 1.0,
                lens_first_rate: 1.0,
            },
        ];
        assert_eq!(mean(results.iter(), |r| r.rate), 0.5);
        assert_eq!(mean(results.iter(), |r| r.strict_rate), 0.5);
        assert_eq!(mean(results.iter(), |r| r.lens_first_rate), 1.0);
    }

    #[test]
    fn chain_metric_credits_recovery_within_window_and_fails_the_flood() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Grep → lens recovery inside the window: pass.
        assert!(lens_within_window(&s(&["Grep", "mcp__lens__lens_skeleton"])));
        // The measured drift signature: fail.
        assert!(!lens_within_window(&s(&["Grep", "Read", "Read"])));
        // Lens engaged only AFTER the window (4th inspection call): fail.
        assert!(!lens_within_window(&s(&[
            "Grep",
            "Read",
            "Read",
            "mcp__lens__lens_search"
        ])));
        // Non-inspection tools don't consume the window.
        assert!(lens_within_window(&s(&[
            "TodoWrite",
            "Grep",
            "Read",
            "mcp__lens__lens_search"
        ])));
        // No tools at all: fail.
        assert!(!lens_within_window(&s(&[])));
    }

    #[test]
    fn seed_tasks_load_and_validate() {
        // `load_tasks` validates every file in the dir, so this also proves the
        // mined tasks (source: "mined:*") parse + validate; we only assert the
        // seeds are still present rather than that nothing else coexists.
        let tasks = load_tasks(&tasks_dir()).expect("all task files should load");
        assert!(tasks.len() >= 3, "expected >= 3 tasks, got {}", tasks.len());
        assert!(
            tasks.iter().filter(|t| t.source == "seed").count() >= 3,
            "expected the >= 3 seed tasks to be present and valid"
        );
    }
}
