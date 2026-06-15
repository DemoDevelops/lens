# /goal: Add session continuity to `ctxforge` (the Context Mode replacement piece)

You are adding **session continuity** to the existing, working `ctxforge` Rust MCP server. This is the capability that lets the user **uninstall Context Mode and RTK and replace them entirely with ctxforge**. Run this AFTER the core build and the benchmark suite are complete and green.

**Read first:** the existing `ctxforge` `src/` (use the real tool names, the real store/SQLite layer, the real `ctx_stats` plumbing — match what exists, do not invent). Also read the core plan and benchmark plan already in the repo for conventions.

---

## 0. The single most important thing to understand before you write code

Session continuity is **NOT** an MCP tool. The core ctxforge server is a passive MCP tool provider — the agent calls tools when it wants. Session continuity is **active**: it must fire on session lifecycle events the agent does not trigger and cannot be relied upon to call a tool for. Specifically, the **PreCompact** moment (right before the conversation compacts) and **every tool call / user prompt** (to capture events as they happen) are lifecycle events, not agent decisions.

Therefore this feature has **two parts**, and they have very different risk:

1. **Session logic (LOW risk, mechanical port):** a SQLite event store, an event extractor, a priority-tiered snapshot builder, and a restore-guide generator. This is straightforward Rust.
2. **Hook layer (HIGHER risk, NEW surface):** ctxforge currently has **no hooks**. You must add a `ctxforge hook <event>` subcommand and install/uninstall commands that register Claude Code hooks (PreToolUse, PostToolUse, UserPromptSubmit, PreCompact, SessionStart) which shell out to that subcommand. This is the part to get right by binding to Claude Code's **actual** hook contract.

**Reference, do not copy:** Context Mode's hook wiring is the reference for *what the Claude Code hook contract is* and *which events to capture* — study it for the contract conventions and the event taxonomy. Write original Rust. (Personal-use project; licensing is not a blocker, but keep a light note of what was borrowed-as-pattern vs. written fresh, in `DECISIONS.md`, in case of future open-sourcing.)

---

## 1. What "done" looks like (the user's actual goal)

After this is built, the user can:
```
ctxforge session install     # registers the lifecycle hooks
# (then uninstall Context Mode + RTK)
```
…and get working-state recovery after compaction that is at least as good as what Context Mode provided — verified by a strict benchmark, not by feel. The swap must be **atomic** (refuses to run alongside Context Mode's hooks to avoid double-fire) and **reversible** (`ctxforge session uninstall` cleanly removes everything).

---

## 2. Part A — Session logic (port, low risk)

### 2.1 Event store
Add a session store (reuse ctxforge's existing SQLite layer; new tables, or a separate `session.db` under the ctxforge data dir — match the existing data-dir convention). Capture these event categories (mirror Context Mode's taxonomy; prioritized):

- **Critical (P1):** file edits/reads/writes, tasks (create/update/complete), plan enter/exit/approve/reject, project rules (CLAUDE.md/AGENTS.md paths+content), user prompts (every message, for last-prompt restore).
- **High (P2):** user decisions/corrections ("use X instead", "don't do Y"), git ops (checkout/commit/merge/rebase/push/pull/status), errors (tool failures, non-zero exits), error→fix resolution pairs, discovered constraints, blockers, rejected approaches, environment changes (cwd, venv, worktree, installs).
- **Normal (P3):** latency outliers, MCP tool usage counts, subagent launches/findings, skills/slash commands, external refs (URLs, #issues), role directives.
- **Low (P4):** session intent classification, large pasted-data references.

Each event row: `session_id, timestamp, category, priority, payload (json), source_hook`.

### 2.2 Snapshot builder (fires at PreCompact)
- Read all events for the current session.
- Build a **priority-tiered snapshot with a hard size budget (≤2 KB target, configurable via `CTXFORGE_SNAPSHOT_BUDGET`)**. If the budget is tight, drop lowest-priority tiers first; **always preserve** active files, tasks, rules, last user prompt, unresolved errors, key decisions.
- Persist the snapshot to a `session_resume` table keyed by session + project.

### 2.3 Restore-guide generator (fires at SessionStart, source=compact/resume)
- Retrieve the stored snapshot.
- Emit a structured **Session Guide** the model receives on resume, with sections: Last Request, Tasks (checkbox w/ status), Plans, Key Decisions, Files Modified, Unresolved Errors (+ error→fix pairs), Constraints, Blockers, Git ops, Project Rules, MCP Tools Used, Subagent findings, Rejected Approaches, External Refs, Environment, Session Intent, User Role.
- Also write the detailed events to ctxforge's FTS5 index so the model can `ctx_search` them on demand (reuse the existing index path).

### 2.4 Session lifecycle semantics
- Fresh session (no `--continue`/`--resume`) = clean slate; prior live events for that project are cleared (match Context Mode's behavior: a fresh session means a clean slate).
- `--continue` / `--resume` / `/resume` = rehydrate from the most recent unconsumed snapshot for the project.

---

## 3. Part B — Hook layer (NEW surface, higher risk)

### 3.1 The hook subcommand
Add `ctxforge hook <platform> <event>` to the binary. It reads the hook payload from stdin (Claude Code passes JSON on stdin), does the right thing per event, and writes the required response on stdout per Claude Code's hook contract:

- **PreToolUse** — capture latency/rejected-approach markers; (optionally) enforce routing later — for now, capture only, do not block.
- **PostToolUse** — extract and store events from the completed tool call (files, git, errors, tasks, etc.).
- **UserPromptSubmit** — capture the user prompt, decisions, blockers, role, intent.
- **PreCompact** — run the snapshot builder (§2.2).
- **SessionStart** — run the restore-guide generator (§2.3) and inject the Session Guide into context per the hook contract.

**Bind to the real Claude Code hook contract.** Read how the installed Claude Code version delivers hook payloads (stdin JSON shape, env vars, how SessionStart injects context, what PreCompact provides). Context Mode's wiring is the reference for the contract; adapt to the actual installed version rather than any assumed shape. If a hook event's exact I/O differs from this plan, follow the real contract and note it in `DECISIONS.md`.

**stdout discipline still applies for the MCP server**, but hook subcommands are separate short-lived invocations — their stdout is the hook response channel. Keep logging on stderr there too.

### 3.2 Install / uninstall (must be atomic + reversible)
- `ctxforge session install` — writes the Claude Code hook configuration (the 5 events above) pointing at `ctxforge hook claude <event>`, using the user's real ctxforge binary path. Embeds the absolute binary path so hooks fire correctly regardless of PATH.
- **Conflict guard (REQUIRED):** before installing, detect whether Context Mode's hooks are present (its hook commands invoke `context-mode hook ...`). If detected, **refuse and print a clear message**: "Context Mode hooks detected — uninstall Context Mode first (`/plugin uninstall context-mode`) to avoid double-firing session hooks." This is what makes the swap atomic; two session-continuity systems on the same lifecycle events will corrupt each other's state.
- `ctxforge session uninstall` — cleanly removes only ctxforge's hook entries, leaving any other hooks intact.
- `ctxforge session status` / extend `ctx_doctor`-style check — report whether hooks are installed, whether a conflicting system is present, FTS5 ok, store ok.

---

## 4. Tests (every piece gets a test; no feature done without one)

- **Event capture:** a simulated PostToolUse payload (file edit / git commit / error) parses into the right category+priority and lands in the store.
- **Snapshot budget:** with many events, the snapshot stays ≤ budget and preserves all P1 items while dropping P4 first. Deterministic given fixed input.
- **Restore guide:** from a known snapshot, the generated guide contains the expected sections and the last user prompt.
- **Lifecycle:** fresh session clears prior live events; resume rehydrates the latest snapshot.
- **Hook subcommand I/O:** feed each hook event a representative stdin payload, assert the stdout response matches the Claude Code contract and the store/snapshot side effects occur.
- **Conflict guard:** install refuses when Context Mode hook entries are present; succeeds when absent; uninstall is clean and leaves unrelated hooks untouched.
- **End-to-end compaction sim:** seed a session with events → run the PreCompact path → run the SessionStart(source=compact) path → assert the model-facing guide reconstructs active files, tasks, last prompt, and unresolved errors.

---

## 5. Benchmark — prove the replacement is at least as good (strict, this is the point)

Add to the existing `benchmarks/` tree a **session-recovery benchmark**. The bar is **Context Mode's behavior**, not ctxforge's own sense of working.

### 5.1 Recovery-fidelity tasks
Build ≥8 scenarios that each: establish a working state (edit files, set tasks, hit and fix an error, make a user decision, run git ops), then force a compaction boundary, then pose a follow-up that is **only answerable correctly if the working state survived** (e.g. "continue editing the file we were on" / "what was the unresolved error" / "what did I tell you to use instead of X").

### 5.2 Three arms, isolated (same per-arm clean-room rule as the main benchmark)
- **No continuity** (floor): no session system installed.
- **Context Mode** (the bar): Context Mode installed, ctxforge session hooks absent.
- **ctxforge** (the candidate): ctxforge session hooks installed, Context Mode uninstalled.

For each arm and scenario record: **did working state survive? (scored against ground truth)** and **snapshot/recovery token cost**.

### 5.3 Output
A recovery table:

| Scenario set | N | No-continuity | Context Mode | ctxforge | Δ (ctxforge − CM) |
| --- | --- | --- | --- | --- | --- |
| File/task recovery | … | … | … | … | … |
| Error/decision recovery | … | … | … | … | … |

**Honest bar:** the claim "ctxforge can replace Context Mode" holds only if **ctxforge ≥ Context Mode** on recovery fidelity at comparable token cost. If ctxforge underperforms on any scenario set, surface it loudly — that's a gap to fix before the user relies on the swap, not a number to massage. Mock-model mode (canned answers) tests the plumbing with no API key; real-model run when `ANTHROPIC_API_KEY` is set.

---

## 6. Definition of done

- `cargo build --release` + `cargo test` green, including all §4 tests and the mock-model recovery harness.
- `ctxforge session install` registers the 5 hooks against the real Claude Code contract, refuses on Context Mode conflict, and `uninstall` cleanly reverses it.
- An end-to-end compaction simulation reconstructs working state (files, tasks, last prompt, unresolved errors) from a real PreCompact→SessionStart cycle.
- The session-recovery benchmark runs (mock without key, real with key) and emits the three-arm recovery table.
- `DECISIONS.md` notes: the real hook contract specifics adapted to, and which parts were pattern-borrowed from Context Mode vs. written fresh.
- Final report states plainly whether **ctxforge ≥ Context Mode** on recovery fidelity. If yes: the user can disable Context Mode + RTK and run ctxforge alone. If not: the exact gaps are listed.

---

## 7. After this lands (note for the user, put in the report)

Once recovery fidelity is proven ≥ Context Mode: the migration is `ctxforge session install` → confirm `session status` clean → `/plugin uninstall context-mode` → remove RTK. RTK's shell-rewrite layer is separately replaceable; if the user still wants it, bundle-and-invoke per the earlier discussion, but it is not required for the Context Mode swap and can stay out of scope here.
