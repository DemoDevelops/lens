//! Snapshot / Session-Guide builder.
//!
//! `build_snapshot` aggregates captured events into a single structured
//! "Session Guide" (the artifact persisted at PreCompact and re-injected at
//! SessionStart). It is bounded by a byte budget: optional tiers are dropped
//! lowest-priority-first, while the must-keep set (last request, tasks, rules,
//! files, unresolved errors, key decisions) is always preserved.

use std::collections::BTreeMap;

use super::Event;

/// Sentinel rank: sections that are never dropped for budget.
const MUST: u32 = u32::MAX;

struct Section {
    /// Drop importance: higher survives longer; [`MUST`] never dropped.
    rank: u32,
    text: String,
}

/// Build the Session Guide from a session's events, bounded by `budget` bytes.
/// `compact_count` is how many times this session has compacted (for the header).
pub fn build_snapshot(events: &[Event], budget: usize, compact_count: i64) -> String {
    let mut sections: Vec<Section> = Vec::new();

    // ── Last Request (must) ──
    if let Some(p) = last_payload(events, "user-prompt", "prompt") {
        sections.push(Section {
            rank: MUST,
            text: section("Last Request", &[cap(&p, 500)]),
        });
    }

    // ── Tasks (must) ── dedupe by text, keep latest status, render checkboxes.
    let tasks = tasks(events);
    if !tasks.is_empty() {
        let lines: Vec<String> = tasks
            .iter()
            .take(20)
            .map(|(t, s)| {
                let box_ = match s.as_str() {
                    "completed" | "done" => "[x]",
                    "in_progress" => "[~]",
                    _ => "[ ]",
                };
                format!("{box_} {} ({s})", cap(t, 120))
            })
            .collect();
        sections.push(Section {
            rank: MUST,
            text: section("Tasks", &lines),
        });
    }

    // ── Plans (optional) ──
    let plans: Vec<String> = events
        .iter()
        .filter(|e| e.category == "plan")
        .filter_map(|e| {
            let action = e
                .payload
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let plan = e.payload.get("plan").and_then(|v| v.as_str()).unwrap_or("");
            if action == "exit" && !plan.is_empty() {
                Some(format!("plan: {}", cap(plan, 240)))
            } else if !action.is_empty() {
                Some(format!("plan {action}"))
            } else {
                None
            }
        })
        .collect();
    if !plans.is_empty() {
        sections.push(Section {
            rank: 50,
            text: section("Plans", &dedup(plans, 5)),
        });
    }

    // ── Key Decisions (must) ──
    let decisions = texts(events, "decision", "text", 8);
    if !decisions.is_empty() {
        sections.push(Section {
            rank: MUST,
            text: section("Key Decisions", &decisions),
        });
    }

    // ── Files Modified (must) ──
    let files = files_modified(events);
    if !files.is_empty() {
        sections.push(Section {
            rank: MUST,
            text: section("Files Modified", &files),
        });
    }

    // ── Unresolved Errors + error→fix pairs (must) ──
    let (unresolved, pairs) = errors(events);
    if !unresolved.is_empty() || !pairs.is_empty() {
        let mut lines = Vec::new();
        for u in unresolved.iter().take(6) {
            lines.push(format!("UNRESOLVED: {}", cap(u, 200)));
        }
        for (err, fix) in pairs.iter().take(4) {
            lines.push(format!("fixed: {} → {}", cap(err, 120), cap(fix, 120)));
        }
        sections.push(Section {
            rank: MUST,
            text: section("Unresolved Errors", &lines),
        });
    }

    // ── Constraints (optional) ──
    let constraints = texts(events, "constraint", "text", 5);
    if !constraints.is_empty() {
        sections.push(Section {
            rank: 60,
            text: section("Constraints", &constraints),
        });
    }

    // ── Blockers (optional) ──
    let blockers = texts(events, "blocker", "text", 5);
    if !blockers.is_empty() {
        sections.push(Section {
            rank: 60,
            text: section("Blockers", &blockers),
        });
    }

    // ── Git ops (optional) ──
    let gits: Vec<String> = events
        .iter()
        .filter(|e| e.category == "git")
        .map(|e| {
            let op = e.payload.get("op").and_then(|v| v.as_str()).unwrap_or("?");
            let cmd = e.payload.get("cmd").and_then(|v| v.as_str()).unwrap_or("");
            format!("{op}: {}", cap(cmd, 100))
        })
        .collect();
    if !gits.is_empty() {
        sections.push(Section {
            rank: 40,
            text: section("Git ops", &dedup(gits, 8)),
        });
    }

    // ── Project Rules (must) — paths only; full content lives in FTS index ──
    let rules: Vec<String> = events
        .iter()
        .filter(|e| e.category == "rule")
        .filter_map(|e| {
            e.payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !rules.is_empty() {
        sections.push(Section {
            rank: MUST,
            text: section("Project Rules", &dedup(rules, 6)),
        });
    }

    // ── MCP Tools Used (optional) — counts ──
    let mcp = counts(events, "mcp-tool", "tool");
    if !mcp.is_empty() {
        let lines: Vec<String> = mcp.iter().map(|(k, n)| format!("{k} ×{n}")).collect();
        sections.push(Section {
            rank: 20,
            text: section("MCP Tools Used", &lines),
        });
    }

    // ── Subagent findings (optional) ──
    let findings: Vec<String> = events
        .iter()
        .filter(|e| e.category == "subagent")
        .filter_map(|e| {
            e.payload
                .get("finding")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !findings.is_empty() {
        sections.push(Section {
            rank: 20,
            text: section("Subagent findings", &dedup(findings, 4)),
        });
    }

    // ── Rejected Approaches (optional) ──
    let rejected = texts(events, "rejected-approach", "text", 5);
    if !rejected.is_empty() {
        sections.push(Section {
            rank: 55,
            text: section("Rejected Approaches", &rejected),
        });
    }

    // ── External Refs (optional) ──
    let refs: Vec<String> = events
        .iter()
        .filter(|e| e.category == "external-ref")
        .filter_map(|e| {
            e.payload
                .get("ref")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !refs.is_empty() {
        sections.push(Section {
            rank: 15,
            text: section("External Refs", &dedup(refs, 8)),
        });
    }

    // ── Environment (optional) ──
    let envs: Vec<String> = events
        .iter()
        .filter(|e| e.category == "environment")
        .filter_map(|e| {
            e.payload
                .get("cmd")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    if !envs.is_empty() {
        sections.push(Section {
            rank: 45,
            text: section("Environment", &dedup(envs, 5)),
        });
    }

    // ── User Role (optional) ──
    if let Some(role) = last_payload(events, "role", "text") {
        sections.push(Section {
            rank: 30,
            text: section("User Role", &[cap(&role, 200)]),
        });
    }

    // ── Session Intent (optional, lowest) ──
    let intent = counts(events, "intent", "intent");
    if let Some((top, _)) = intent.first() {
        sections.push(Section {
            rank: 10,
            text: section("Session Intent", std::slice::from_ref(top)),
        });
    }

    render(sections, budget, compact_count)
}

fn render(mut sections: Vec<Section>, budget: usize, compact_count: i64) -> String {
    let header = if compact_count > 0 {
        format!("# Session Guide (resumed after {compact_count} compaction(s))\n")
    } else {
        "# Session Guide\n".to_string()
    };

    let assemble = |secs: &[Section]| -> String {
        let mut out = header.clone();
        for s in secs {
            out.push('\n');
            out.push_str(&s.text);
        }
        out
    };

    // Drop optional sections (lowest rank first) until within budget.
    loop {
        let out = assemble(&sections);
        if out.len() <= budget {
            return out;
        }
        // Find the lowest-rank droppable section.
        let victim = sections
            .iter()
            .enumerate()
            .filter(|(_, s)| s.rank != MUST)
            .min_by_key(|(_, s)| s.rank)
            .map(|(i, _)| i);
        match victim {
            Some(i) => {
                sections.remove(i);
            }
            None => return assemble(&sections), // only must-keep left; return as-is
        }
    }
}

// ── aggregation helpers ──────────────────────────────────────────────────────

fn section(title: &str, lines: &[String]) -> String {
    let mut s = format!("## {title}\n");
    for l in lines {
        s.push_str("- ");
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// Latest payload string for a category/field.
fn last_payload(events: &[Event], category: &str, field: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .find(|e| e.category == category)
        .and_then(|e| {
            e.payload
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
}

/// Distinct text values for a category (latest-last, capped).
fn texts(events: &[Event], category: &str, field: &str, max: usize) -> Vec<String> {
    let v: Vec<String> = events
        .iter()
        .filter(|e| e.category == category)
        .filter_map(|e| {
            e.payload
                .get(field)
                .and_then(|x| x.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    dedup(v, max)
}

/// Tasks deduped by text, keeping the latest status seen.
fn tasks(events: &[Event]) -> Vec<(String, String)> {
    let mut order: Vec<String> = Vec::new();
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    for e in events.iter().filter(|e| e.category == "task") {
        let t = e.payload.get("task").and_then(|v| v.as_str()).unwrap_or("");
        let s = e
            .payload
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("pending");
        if t.is_empty() {
            continue;
        }
        if !map.contains_key(t) {
            order.push(t.to_string());
        }
        map.insert(t.to_string(), s.to_string());
    }
    order
        .into_iter()
        .map(|t| {
            let s = map[&t].clone();
            (t, s)
        })
        .collect()
}

/// Edited/written file paths, deduped (most-recent order preserved).
fn files_modified(events: &[Event]) -> Vec<String> {
    let v: Vec<String> = events
        .iter()
        .filter(|e| e.category == "file")
        .filter(|e| {
            matches!(
                e.payload.get("action").and_then(|v| v.as_str()),
                Some("edit") | Some("write")
            )
        })
        .filter_map(|e| {
            e.payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    dedup(v, 15)
}

/// Returns (unresolved error messages, resolved error→fix pairs).
///
/// Heuristic: an error is resolved if a file edit/write or a git commit occurs
/// after it; errors after the last such progress event are unresolved.
fn errors(events: &[Event]) -> (Vec<String>, Vec<(String, String)>) {
    let is_progress = |e: &Event| -> Option<String> {
        match e.category.as_str() {
            "file"
                if matches!(
                    e.payload.get("action").and_then(|v| v.as_str()),
                    Some("edit") | Some("write")
                ) =>
            {
                e.payload
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map(|p| format!("edited {p}"))
            }
            "git" if e.payload.get("op").and_then(|v| v.as_str()) == Some("commit") => {
                Some("git commit".to_string())
            }
            _ => None,
        }
    };

    let mut unresolved = Vec::new();
    let mut pairs = Vec::new();
    for (i, e) in events.iter().enumerate() {
        if e.category != "error" {
            continue;
        }
        let msg = e
            .payload
            .get("message")
            .and_then(|v| v.as_str())
            .or_else(|| e.payload.get("cmd").and_then(|v| v.as_str()))
            .unwrap_or("error")
            .to_string();
        let fix = events.iter().skip(i + 1).find_map(is_progress);
        match fix {
            Some(f) => pairs.push((msg, f)),
            None => unresolved.push(msg),
        }
    }
    (unresolved, pairs)
}

/// Value frequency for a category/field, most-frequent first.
fn counts(events: &[Event], category: &str, field: &str) -> Vec<(String, usize)> {
    let mut map: BTreeMap<String, usize> = BTreeMap::new();
    for e in events.iter().filter(|e| e.category == category) {
        if let Some(v) = e.payload.get(field).and_then(|x| x.as_str()) {
            *map.entry(v.to_string()).or_insert(0) += 1;
        }
    }
    let mut v: Vec<(String, usize)> = map.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

/// Order-preserving dedup, capped to the most recent `max`.
fn dedup(items: Vec<String>, max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for it in items.into_iter().rev() {
        if seen.insert(it.clone()) {
            out.push(it);
        }
    }
    out.reverse();
    if out.len() > max {
        out = out.split_off(out.len() - max);
    }
    out
}

fn cap(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    let s = s.trim();
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn e(cat: &str, prio: u8, payload: serde_json::Value, ts: i64) -> Event {
        Event {
            session_id: "s".into(),
            project: "/p".into(),
            timestamp: ts,
            category: cat.into(),
            priority: prio,
            payload,
            source_hook: "x".into(),
        }
    }

    fn rich_events() -> Vec<Event> {
        vec![
            e("user-prompt", 1, json!({"prompt": "fix the auth bug"}), 1),
            e("rule", 1, json!({"path": "/p/CLAUDE.md"}), 2),
            e(
                "file",
                1,
                json!({"action": "edit", "path": "src/auth.rs"}),
                3,
            ),
            e(
                "task",
                1,
                json!({"task": "write failing test", "status": "in_progress"}),
                4,
            ),
            e(
                "decision",
                2,
                json!({"text": "use argon2 instead of bcrypt"}),
                5,
            ),
            e("error", 2, json!({"message": "E0433 unresolved import"}), 6),
            e(
                "git",
                2,
                json!({"op": "commit", "cmd": "git commit -m wip"}),
                7,
            ),
            e("mcp-tool", 3, json!({"tool": "mcp__x__y"}), 8),
            e("external-ref", 3, json!({"ref": "#42"}), 9),
            e("intent", 4, json!({"intent": "debug"}), 10),
            e(
                "user-prompt",
                1,
                json!({"prompt": "what was the unresolved error?"}),
                11,
            ),
        ]
    }

    #[test]
    fn guide_contains_expected_sections_and_last_prompt() {
        let g = build_snapshot(&rich_events(), 4096, 1);
        assert!(g.contains("Session Guide (resumed after 1 compaction"));
        assert!(g.contains("## Last Request"));
        assert!(g.contains("what was the unresolved error?")); // latest prompt
        assert!(g.contains("## Tasks"));
        assert!(g.contains("[~] write failing test"));
        assert!(g.contains("## Key Decisions"));
        assert!(g.contains("argon2"));
        assert!(g.contains("## Files Modified"));
        assert!(g.contains("src/auth.rs"));
        assert!(g.contains("## Project Rules"));
        assert!(g.contains("CLAUDE.md"));
    }

    #[test]
    fn unresolved_error_surfaces_when_no_later_progress() {
        // error is the last event → unresolved
        let evs = vec![
            e("file", 1, json!({"action": "edit", "path": "a.rs"}), 1),
            e("error", 2, json!({"message": "boom at runtime"}), 2),
        ];
        let g = build_snapshot(&evs, 4096, 0);
        assert!(g.contains("UNRESOLVED: boom at runtime"));
    }

    #[test]
    fn error_then_edit_becomes_resolved_pair() {
        let evs = vec![
            e("error", 2, json!({"message": "compile fail"}), 1),
            e("file", 1, json!({"action": "edit", "path": "fix.rs"}), 2),
        ];
        let g = build_snapshot(&evs, 4096, 0);
        assert!(g.contains("fixed: compile fail → edited fix.rs"));
        assert!(!g.contains("UNRESOLVED"));
    }

    #[test]
    fn budget_drops_low_priority_first_keeps_p1() {
        // Tight budget: lowest-priority optional tiers must be dropped first,
        // but every must-keep P1 section survives. (Must-only floor is ~300B;
        // full guide is ~426B, so 360 forces partial dropping.)
        let g = build_snapshot(&rich_events(), 360, 0);
        assert!(g.len() <= 360, "len {} > budget", g.len());
        // must-keep preserved
        assert!(g.contains("## Last Request"));
        assert!(g.contains("## Tasks"));
        assert!(g.contains("## Files Modified"));
        assert!(g.contains("## Key Decisions"));
        assert!(g.contains("## Unresolved Errors") || g.contains("E0433"));
        assert!(g.contains("## Project Rules"));
        // lowest-priority dropped first; a higher-ranked optional survives.
        assert!(
            !g.contains("## Session Intent"),
            "P4 intent should drop first"
        );
        assert!(!g.contains("## External Refs"));
        assert!(
            g.contains("## Git ops"),
            "higher-rank optional should survive"
        );
    }

    #[test]
    fn deterministic_for_fixed_input() {
        let a = build_snapshot(&rich_events(), 2048, 1);
        let b = build_snapshot(&rich_events(), 2048, 1);
        assert_eq!(a, b);
    }
}
