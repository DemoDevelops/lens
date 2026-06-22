//! Event extraction: turn a completed tool call or a user prompt into
//! categorized, prioritized [`RawEvent`]s. Pure functions, no I/O, so they are
//! cheap to unit-test against representative payloads.

use serde_json::{json, Value};

use super::RawEvent;

/// Extract events from a completed tool call (PostToolUse).
///
/// `tool_input` is the tool's argument object; `tool_response` is its result
/// rendered as a string (Claude Code passes either a string or an object).
pub fn extract_tool_events(
    tool_name: &str,
    tool_input: &Value,
    tool_response: &str,
) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let name = tool_name;

    match name {
        "Edit" | "MultiEdit" | "NotebookEdit" => {
            if let Some(p) = str_field(tool_input, "file_path")
                .or_else(|| str_field(tool_input, "notebook_path"))
            {
                out.push(RawEvent::new(
                    "file",
                    1,
                    json!({"action": "edit", "path": p}),
                ));
            }
        }
        "Write" => {
            if let Some(p) = str_field(tool_input, "file_path") {
                out.push(RawEvent::new(
                    "file",
                    1,
                    json!({"action": "write", "path": p}),
                ));
            }
        }
        "Read" => {
            if let Some(p) = str_field(tool_input, "file_path") {
                out.push(RawEvent::new(
                    "file",
                    1,
                    json!({"action": "read", "path": p}),
                ));
            }
        }
        "TodoWrite" | "TaskCreate" | "TaskUpdate" => {
            out.extend(extract_tasks(tool_input));
        }
        "EnterPlanMode" => out.push(RawEvent::new("plan", 1, json!({"action": "enter"}))),
        "ExitPlanMode" => {
            let plan = str_field(tool_input, "plan").unwrap_or_default();
            out.push(RawEvent::new(
                "plan",
                1,
                json!({"action": "exit", "plan": truncate(&plan, 600)}),
            ));
        }
        "Bash" => out.extend(extract_bash(tool_input, tool_response)),
        "Skill" => {
            if let Some(s) = str_field(tool_input, "skill") {
                out.push(RawEvent::new("skill", 3, json!({"skill": s})));
            }
        }
        "Agent" | "Task" => {
            let desc = str_field(tool_input, "description").unwrap_or_default();
            out.push(RawEvent::new("subagent", 3, json!({"launched": desc})));
            let finding = first_line(tool_response);
            if !finding.is_empty() {
                out.push(RawEvent::new(
                    "subagent",
                    3,
                    json!({"finding": truncate(&finding, 300)}),
                ));
            }
        }
        _ => {
            if name.starts_with("mcp__") {
                out.push(RawEvent::new("mcp-tool", 3, json!({"tool": name})));
            }
        }
    }

    // Generic error detection from any tool response.
    if response_looks_like_error(tool_response) && name != "Bash" {
        out.push(RawEvent::new(
            "error",
            2,
            json!({"tool": name, "message": truncate(&first_line(tool_response), 300)}),
        ));
    }

    out
}

fn extract_tasks(tool_input: &Value) -> Vec<RawEvent> {
    let mut out = Vec::new();
    // TodoWrite: { todos: [{content, status, ...}] }
    if let Some(todos) = tool_input.get("todos").and_then(|v| v.as_array()) {
        for t in todos {
            let content = str_field(t, "content").unwrap_or_default();
            let status = str_field(t, "status").unwrap_or_else(|| "pending".into());
            if !content.is_empty() {
                out.push(RawEvent::new(
                    "task",
                    1,
                    json!({"task": content, "status": status}),
                ));
            }
        }
        return out;
    }
    // TaskCreate / TaskUpdate single
    let content = str_field(tool_input, "description")
        .or_else(|| str_field(tool_input, "content"))
        .or_else(|| str_field(tool_input, "prompt"))
        .unwrap_or_default();
    let status = str_field(tool_input, "status").unwrap_or_else(|| "pending".into());
    if !content.is_empty() {
        out.push(RawEvent::new(
            "task",
            1,
            json!({"task": truncate(&content, 200), "status": status}),
        ));
    }
    out
}

fn extract_bash(tool_input: &Value, tool_response: &str) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let cmd = str_field(tool_input, "command").unwrap_or_default();
    let low = cmd.to_lowercase();
    let first = cmd.split_whitespace().next().unwrap_or("");

    // git operations (P2)
    if first == "git" || low.starts_with("rtk git") {
        let op = git_op(&low);
        out.push(RawEvent::new(
            "git",
            2,
            json!({"op": op, "cmd": truncate(&cmd, 200)}),
        ));
    }

    // environment changes (P2): cwd, venv, installs, worktree
    if low.starts_with("cd ")
        || low.contains("activate")
        || low.contains("npm install")
        || low.contains("pip install")
        || low.contains("cargo add")
        || low.contains("worktree")
        || low.contains("export ")
    {
        out.push(RawEvent::new(
            "environment",
            2,
            json!({"cmd": truncate(&cmd, 200)}),
        ));
    }

    // errors (P2): non-zero markers in output
    if response_looks_like_error(tool_response) {
        out.push(RawEvent::new(
            "error",
            2,
            json!({"cmd": truncate(&cmd, 120), "message": truncate(&first_line(tool_response), 300)}),
        ));
    }

    out
}

fn git_op(low: &str) -> &'static str {
    for op in [
        "commit", "checkout", "merge", "rebase", "push", "pull", "status", "branch", "stash",
    ] {
        if low.contains(op) {
            return match op {
                "commit" => "commit",
                "checkout" => "checkout",
                "merge" => "merge",
                "rebase" => "rebase",
                "push" => "push",
                "pull" => "pull",
                "branch" => "branch",
                "stash" => "stash",
                _ => "status",
            };
        }
    }
    "other"
}

/// Extract events from a user prompt (UserPromptSubmit).
pub fn extract_user_events(prompt: &str) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let trimmed = prompt.trim();
    if trimmed.is_empty() {
        return out;
    }

    // Always capture the raw prompt (P1) — last-prompt restore depends on it.
    out.push(RawEvent::new("user-prompt", 1, json!({"prompt": trimmed})));

    let low = trimmed.to_lowercase();

    // Decisions / corrections (P2)
    let decision_markers = [
        " instead",
        "don't ",
        "do not ",
        "use ",
        "stop ",
        "actually ",
        "rather than",
        "never ",
        "always ",
        "make sure",
    ];
    if decision_markers.iter().any(|m| low.contains(m)) {
        out.push(RawEvent::new(
            "decision",
            2,
            json!({"text": truncate(trimmed, 300)}),
        ));
    }

    // Blockers (P2)
    if low.contains("blocked")
        || low.contains("waiting on")
        || low.contains("can't ")
        || low.contains("cannot ")
    {
        out.push(RawEvent::new(
            "blocker",
            2,
            json!({"text": truncate(trimmed, 240)}),
        ));
    }

    // Constraints (P2)
    if low.contains("must ")
        || low.contains("required")
        || low.contains("constraint")
        || low.contains("has to ")
    {
        out.push(RawEvent::new(
            "constraint",
            2,
            json!({"text": truncate(trimmed, 240)}),
        ));
    }

    // Role directives (P3)
    if low.contains("you are ") || low.contains("act as ") || low.contains("your role") {
        out.push(RawEvent::new(
            "role",
            3,
            json!({"text": truncate(trimmed, 200)}),
        ));
    }

    // External refs (P3): URLs and #issue tokens
    for r in find_external_refs(trimmed) {
        out.push(RawEvent::new("external-ref", 3, json!({"ref": r})));
    }

    // Session intent (P4): coarse classification
    let intent = classify_intent(&low);
    out.push(RawEvent::new("intent", 4, json!({"intent": intent})));

    out
}

fn classify_intent(low: &str) -> &'static str {
    if low.contains("fix") || low.contains("bug") || low.contains("error") || low.contains("broken")
    {
        "debug"
    } else if low.contains("add")
        || low.contains("implement")
        || low.contains("build")
        || low.contains("create")
    {
        "feature"
    } else if low.contains("refactor") || low.contains("clean") || low.contains("simplify") {
        "refactor"
    } else if low.contains("test") {
        "testing"
    } else if low.contains("?")
        || low.contains("explain")
        || low.contains("how ")
        || low.contains("what ")
    {
        "question"
    } else {
        "general"
    }
}

fn find_external_refs(text: &str) -> Vec<String> {
    let mut refs = Vec::new();
    for tok in text.split_whitespace() {
        let t = tok.trim_matches(|c: char| {
            !c.is_alphanumeric() && c != ':' && c != '/' && c != '#' && c != '.'
        });
        if t.starts_with("http://") || t.starts_with("https://") {
            refs.push(t.to_string());
        } else if t.starts_with('#') && t.len() > 1 && t[1..].chars().all(|c| c.is_ascii_digit()) {
            refs.push(t.to_string());
        }
    }
    refs.truncate(5);
    refs
}

// ── helpers ────────────────────────────────────────────────────────────────

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn first_line(s: &str) -> String {
    s.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// Heuristic: does a tool response read like a failure? Best-effort; the model
/// re-verifies on resume, so a false positive only adds a low-cost line.
fn response_looks_like_error(resp: &str) -> bool {
    let low = resp.to_lowercase();
    low.contains("error")
        || low.contains("traceback")
        || low.contains("exception")
        || low.contains("command not found")
        || low.contains("no such file")
        || low.contains("failed")
        || low.contains("panicked")
        || low.contains("fatal:")
        || low.contains("cannot find")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_becomes_p1_file_event() {
        let ev = extract_tool_events("Edit", &json!({"file_path": "src/a.rs"}), "ok");
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].category, "file");
        assert_eq!(ev[0].priority, 1);
        assert_eq!(ev[0].payload["action"], "edit");
        assert_eq!(ev[0].payload["path"], "src/a.rs");
    }

    #[test]
    fn git_commit_is_p2_git() {
        let ev = extract_tool_events("Bash", &json!({"command": "git commit -m wip"}), "");
        assert!(ev
            .iter()
            .any(|e| e.category == "git" && e.priority == 2 && e.payload["op"] == "commit"));
    }

    #[test]
    fn bash_error_output_makes_error_event() {
        let ev = extract_tool_events(
            "Bash",
            &json!({"command": "cargo build"}),
            "error[E0433]: failed to resolve",
        );
        assert!(ev.iter().any(|e| e.category == "error" && e.priority == 2));
    }

    #[test]
    fn todowrite_yields_tasks_with_status() {
        let ev = extract_tool_events(
            "TodoWrite",
            &json!({"todos": [
                {"content": "write tests", "status": "in_progress"},
                {"content": "ship", "status": "pending"}
            ]}),
            "",
        );
        let tasks: Vec<_> = ev.iter().filter(|e| e.category == "task").collect();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].payload["status"], "in_progress");
    }

    #[test]
    fn user_decision_and_prompt_extracted() {
        let ev = extract_user_events("Use ripgrep instead of grep, and don't touch config.");
        assert!(ev
            .iter()
            .any(|e| e.category == "user-prompt" && e.priority == 1));
        assert!(ev
            .iter()
            .any(|e| e.category == "decision" && e.priority == 2));
    }

    #[test]
    fn external_refs_found() {
        let ev = extract_user_events("see https://example.com/x and issue #123 please");
        let refs: Vec<_> = ev.iter().filter(|e| e.category == "external-ref").collect();
        assert_eq!(refs.len(), 2);
    }

    #[test]
    fn mcp_tool_usage_counted() {
        let ev = extract_tool_events("mcp__foo__bar", &json!({}), "ok");
        assert!(ev
            .iter()
            .any(|e| e.category == "mcp-tool" && e.priority == 3));
    }
}
