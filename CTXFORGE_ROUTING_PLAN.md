# ctxforge auto-routing design spec — make the tools get used

## 0. Problem

ctxforge's token-saving tools (`ctx_execute`, `ctx_search`, `ctx_discover`/`graph_*`)
are **opt-in MCP tools**: they save tokens only when the model *chooses* to call
them. Observed live: a large Meridian goal ran 314+ built-in tool actions
(Read/Edit/Bash, captured by the session hooks in `session.db`) and **zero**
ctxforge MCP calls — so `ops.log` stayed empty and the dashboard's savings panel
never moved. Having ctxforge connected ≠ ctxforge being used.

To get automatic savings we need **interception at the hook layer**, the way the
projects we drew from do it. ctxforge already registers all five lifecycle hooks
(`PreToolUse` is currently a capture-only stub: `// no routing enforcement`), so
the seam exists; this spec defines what to put in it.

**Scope note:** RTK and Context Mode are **removed** from the live config
(`CLAUDE_CONFIG_DIR=~/.claude-personal`; `rtk` not on PATH; `context-mode` not
enabled). The Context Mode plugin source remains on disk only as a *reference*.
So ctxforge can own all tool routing with **no coexistence constraint** — the
earlier "RTK owns Bash" concern is void.

---

## 1. How the inspiration projects handle it (vs our capabilities)

| Project | License | Mechanism | What it touches | How it saves | Already in ctxforge? |
|---|---|---|---|---|---|
| **Context Mode** | ELv2 (study only) | **Steer**: SessionStart injects a routing rulebook (`additionalContext`); PreToolUse router returns `deny`/`modify`/`context`; re-inject on compact | Bash, Read, Grep, WebFetch, Agent | model voluntarily re-issues work through MCP sandbox tools | Hooks exist but capture-only; have `ctx_execute`/`ctx_search`/discovery; **missing** `batch_execute`/`execute_file`/`fetch_and_index` |
| **RTK** | (removed; infer from docs) | **Rewrite**: PreToolUse(Bash) rewrites `<cmd>` → `rtk <cmd>` proxy via `updatedInput` | Bash (curated high-output commands) | proxy runs the command and returns compressed output (60–90%); transparent | No proxy/`wrap` mode; have the sandbox+store that a wrapper would reuse |
| **graphify** | MIT (ideas) | **Represent**: offline tree-sitter AST → structural graph; query a subgraph | n/a (not interception) | agent queries scoped subgraph instead of reading files | **Fully ported** (`ctx_discover` + `graph_*`) |
| **headroom** | Apache-2.0 (ideas) | **Compact**: SmartCrusher columnar transpose + dict-encode of homogeneous JSON | n/a (not interception) | lossless structural compaction of large JSON | **Ported** (`store/compress.rs`) |

**Reading:** only two of the four are *interception* strategies. graphify and
headroom are *representation/compaction* and are already in. The routing question
is a choice between Context Mode's **steer** and RTK's **rewrite** — or a hybrid.

### 1a. Context Mode wiring, precisely (the reference)

- **SessionStart** (`sessionstart.mjs`) emits `hookSpecificOutput.additionalContext`
  = a `<context_window_protection>` block: a tool-selection hierarchy
  ("GATHER → ctx_batch_execute; FOLLOW-UP → ctx_search; PROCESSING → ctx_execute")
  plus `<forbidden_actions>` ("NO Bash for commands producing >20 lines output",
  "NO Read for analysis — use ctx_execute_file", "NO WebFetch"). Re-appended on
  `compact`/`resume` with a budget-capped (~500 token) behavioral state block so
  steering survives compaction.
- **PreToolUse** (`pretooluse.mjs` → `core/routing.mjs`) returns a normalized
  decision; `core/formatters.mjs` maps it to Claude Code JSON:

  | decision | Claude Code output |
  |---|---|
  | `deny` | `{hookSpecificOutput:{hookEventName:"PreToolUse",permissionDecision:"deny",permissionDecisionReason}}` |
  | `ask` | `{…,permissionDecision:"ask"}` |
  | `modify` | `{…,permissionDecision:"allow",permissionDecisionReason,updatedInput}}` |
  | `context` | `{hookSpecificOutput:{hookEventName:"PreToolUse",additionalContext}}` |
  | passthrough | `null` |

  Routing rules: WebFetch → `deny` + steer; curl/wget/inline-HTTP/build-tools in
  Bash → `modify` (rewrite command to an `echo "use ctx_execute…"` — a *soft*
  block, not a real wrap); generic Bash/Read/Grep → `context` nudge, **throttled
  once per session per tool** via `O_EXCL` marker files; an **MCP-ready guard**
  (`isMCPReady()`) makes redirects fall through to passthrough if the MCP server
  is down (never wedge the agent).
- **PostToolUse** captures events + records that a redirect happened. It does
  **not** modify output. Notably, `updatedToolOutput` is **not used anywhere** in
  Context Mode — confirming the proven path is *steer*, not *rewrite-output*.

### 1b. ctxforge capabilities and gaps

Have: `ctx_execute` (sandbox: python/js/ts/bash/ruby/go; large stdout → store +
preview + `retrieve_ref`), `ctx_retrieve`, `ctx_index`+`ctx_search` (FTS5),
`ctx_discover`+`graph_*`, `ctx_stats`; the reversible store; the five hooks; and
the new observability layer (`ops.log`, `ctxforge stats`/`verify`/`dashboard`).

Gaps vs Context Mode's steering targets:
- **No `ctx_execute_file`** — the "read+analyze a file in sandbox" tool Read is
  steered toward. `ctx_execute` can `open()` a file in code, but there's no
  one-call ergonomic form.
- **No `ctx_batch_execute`** — run N commands + auto-index + search in one call.
- **No `ctx_fetch_and_index`** — fetch URL + index. `ctx_execute` can fetch.
- **`ctx_search` requires a prior `ctx_index`**; Context Mode auto-indexes via
  `batch_execute`, so its "just search" guidance works out of the box.

Steering to tools that are clumsier than the native tool will fail: the model
will route around friction. So **tool ergonomics is part of the routing design**,
not separable from it.

---

## 2. Recommended approach: capability-aware hybrid

Neither pure strategy fits ctxforge's surface. Recommendation:

- **Bash → RTK-style selective transparent wrap.** Rewrite (`updatedInput`)
  output-heavy *read-only* commands to `ctxforge wrap -- <cmd>`, a new subcommand
  that runs the command, offloads large stdout to the reversible store, and
  returns a head+tail preview + `retrieve_ref` (reusing the exact sandbox/store
  path). **Deterministic savings with no reliance on model compliance**, and it
  writes an `ops.log` record so the dashboard finally moves.
  - **Only an allowlist of read-only, high-output commands** (find, cat, ls -R,
    rg/grep, tail/head of logs, curl/wget, build tools: gradle/mvn/sbt, test
    runners, `git log`/`git diff`). **Never wrap stateful/navigation commands**
    (`cd`, `export`, `source`, alias, function defs, `&&`-chains containing them)
    — wrapping them in a subshell would break Claude Code's persistent-shell cwd
    /env state. This is the key hazard RTK sidesteps by proxying a curated set.
- **Read → steer (nudge), never block.** Most Reads are read-to-edit (legit,
  must stay native). Read-to-analyze nudges toward `ctx_execute_file`. **Plus
  graph escalation (shipped 2026-06-20):** code-reads also escalate toward the
  graph — the first code read names `graph_query`/`graph_neighbors`/`graph_path`
  among its options, and after 3 cumulative reads of graph-indexed files
  (`spec_for_extension`: rs/py/js/ts/go/swift) a graph-specific nudge fires,
  repeating every 3rd read. Targets the "reading file after file to trace
  structure" pattern the graph replaces. Not in the original plan; see note below.
- **Grep → steer (nudge)** toward `ctx_execute` running ripgrep in-sandbox for
  large result sets.
- **WebFetch → `deny` + steer** to `ctx_execute` (fetch+process in sandbox).
  Network responses are reliably large; deny is justified, with a clear reason.
- **SessionStart → inject ctxforge routing block** (original prose) alongside the
  existing Session Guide, re-injected on compact (the seam already exists).

This gives **deterministic savings on the biggest, safest category (Bash output)**
without trusting the model, and **lossless steering** on the categories that
can't be transparently wrapped — adapted to ctxforge's actual tools.

### 2a. Tool work (so steering has somewhere good to go)

- **`ctx_execute_file(path, language, code)`** — thin wrapper over the sandbox
  that injects the file path; makes the Read nudge land on a real, ergonomic tool.
  Now live (Read steering is on).
- **`ctx_batch_execute`** — run N labeled commands, auto-index, return a summary.
  **Deferred** until `ops.log` data shows multi-command round-trips dominate.

> **Finding (2026-06-20):** `ops.log` showed the graph getting ~zero agent use
> (`graph_query` 2/86, both warmup; `graph_neighbors`/`graph_path` 0) while
> `ctx_execute_file` absorbed the navigational reads. Cause: the Read nudge only
> pointed at `ctx_execute_file`, never the graph — the just-in-time signal at the
> exact moment of a code read steered every "understand this code" read into
> single-file analysis. Fix: the graph escalation above. Lesson: a tool needs a
> nudge at its decision point, not just a mention in the SessionStart block.

### 2b. Lossless + observable by construction

- Wrapped/offloaded output always writes the full blob to the reversible store;
  `ctx_retrieve <ref>` recovers it byte-for-byte (already verified by
  `ctxforge verify --roundtrip`). Nothing is lost.
- Every wrap/redirect writes an `ops.log` record (tool=`bash_wrap` etc.), so the
  "first plane" (built-in tool use) now produces real savings **and** shows on the
  dashboard — closing the gap this whole investigation surfaced.

### 2c. Controls

- **Env-gated, default OFF**: `CTXFORGE_ROUTING=off|steer|wrap|full` (default
  `off`). `steer` = nudges/deny only; `wrap` = Bash transparent wrap only; `full`
  = both. The four levels ARE the phased rollout (§6): all code ships at once
  behind the flag; you flip the level to roll out. Read steering is inside
  `steer`/`full` but only takes effect once `ctx_execute_file` exists (Phase 3).
- **MCP-ready guard**: if the ctxforge MCP server isn't reachable, all redirects
  fall through to passthrough (mirror Context Mode's `mcpRedirect`).
- **Throttle**: once-per-session per nudge type, via `$CTXFORGE_DIR` marker files
  (atomic create), so nudges don't spam a long session.
- **License**: re-author all routing logic and steering prose originally in Rust.
  Study the ELv2 Context Mode source for the *pattern* only; do not copy it. The
  Claude Code hook JSON shape is public API (not copyrightable).

---

## 3. Risks / tradeoffs

- **Model compliance (steer):** nudges can be ignored. Mitigated by making Bash
  (the high-volume case) deterministic via wrap, and by the SessionStart rulebook.
- **Shell-state hazard (wrap):** wrapping `cd`/`export`/chains breaks persistent
  shell state. Mitigated by the strict read-only allowlist; everything else passes
  through untouched.
- **Read sensitivity:** never block Read; nudge only. A wrong block here stalls
  real work.
- **Deny UX:** `permissionDecision:"deny"` surfaces to the user. Reserve it for
  WebFetch (clear win) and keep reasons short and instructive.
- **Wrap edge cases:** interactive/TTY commands, stdin, exit-code fidelity,
  quoting/escaping of the rewritten command. The wrap subcommand must preserve
  exit code, stream stderr, and run via `sh -c` to keep shell semantics.
- **Double counting:** a wrapped Bash op writes both a `session.db` event (hook)
  and an `ops.log` record; the dashboard already separates the two planes.

---

## 4. Verification

- Each routed tool produces the exact Claude Code hook JSON (assert per decision
  type against a golden payload).
- `ctxforge wrap`: small output passes through verbatim (zero behavior change);
  large output offloads, returns preview + `ref`, and `ctx_retrieve <ref>`
  reproduces the original byte-for-byte (`verify --roundtrip` PASS).
- Allowlist: `cd x && big_cmd` is **not** wrapped (state preserved); `find /` **is**.
- Throttle: a nudge type fires at most once per session.
- MCP-ready guard: with the server down, every decision is passthrough.
- `CTXFORGE_ROUTING=off` ⇒ PreToolUse output identical to today (empty), proving
  default-off is a true no-op.
- After a wrapped Bash op, `ctxforge stats`/dashboard show a non-zero savings op.
- `cargo test` green; clean build; MCP server stdout stays pure JSON-RPC.

---

## 5. Task graph (parallel-safe)

Tasks are partitioned by **disjoint file ownership** so a wave's tasks can run
concurrently in separate worktrees without merge conflicts. "Read-only" files may
be read by a task but are owned (written) by exactly one task across the whole graph.

| ID | Title | Depends on | Writes (owned) | Read-only | Subagent | Completion predicate |
|----|-------|-----------|----------------|-----------|----------|----------------------|
| T0 | Scaffold module seams | none | `src/lib.rs` (+`pub mod routing; pub mod wrap;`), `src/main.rs` (+`wrap` dispatch), stub `src/routing/mod.rs`, stub `src/wrap.rs` | — | implement | `cargo build` green with stubs; `ctxforge wrap` runs and prints usage/no-op |
| T1 | Routing core + hook wiring | T0 | `src/routing/mod.rs` (decision enum, per-tool policy, Bash allowlist with git-subcommand awareness + stateful-command exclusion, decision→Claude-Code JSON formatter, `CTXFORGE_ROUTING` level gate, once-per-session throttle, MCP-ready guard), `src/session/hook.rs` (PreToolUse → router; SessionStart → inject routing block, re-inject on compact) | `src/obs` | implement | `cargo test routing::` green; integration: `=off`→`{}`; `=steer`→WebFetch deny JSON + Bash nudge fires once then passthrough; `=full`→wrappable Bash → `updatedInput.command="…ctxforge wrap -- …"`, `cd x && big` passes through unchanged; SessionStart `additionalContext` contains the tool hierarchy |
| T2 | `ctxforge wrap` subcommand | T0 | `src/wrap.rs` (run via `sh -c`, capture stdout/stderr, preserve exit code, offload large stdout to `Store`, return preview+`retrieve_ref`, write `ops.log` via `obs::OpLog`; own small head+tail preview) | `src/store`, `src/obs` | implement | `ctxforge wrap -- 'python3 -c "print(\"A\"*50000)"'` exits 0, prints preview+ref; small output passes verbatim; `ctxforge verify --roundtrip <ref>` PASS; one `ops.log` record written |
| T3 | `ctx_execute_file` MCP tool | T0 | `src/tools.rs` (request/response structs), `src/server.rs` (handler + op record), `src/sandbox/mod.rs` (run-with-file helper) | — | implement | e2e via rmcp client: `ctx_execute_file(path, language, code)` returns only printed output; large output offloaded with `retrieve_ref` |
| T4 | E2E, purity, docs | T1,T2,T3 | new `tests/routing_tests.rs`, `README.md` (+`CTXFORGE_ROUTING` levels & allowlist) | all `src` | verify | full `cargo test` green; MCP server stdout pure-JSON-RPC test passes; after a wrapped Bash op, `ctxforge stats` shows a non-zero savings op; README documents the four levels |

### Parallel waves

| Wave | Tasks | Concurrency | Notes |
|------|-------|-------------|-------|
| 1 | T0 | 1 | creates the seams so the rest never touch `lib.rs`/`main.rs` |
| 2 | **T1, T2, T3** | 3 | fully disjoint files (routing+hook / wrap / server+tools+sandbox) — run in parallel worktrees |
| 3 | T4 | 1 | integration verification + docs; merges after T1–T3 land |

---

## 6. Locked decisions

1. **Strategy: capability-aware hybrid** — Bash transparent wrap (deterministic,
   no model reliance) + steer (deny/nudge) for what can't be wrapped. Build all of
   it; roll out by env level.
2. **Rollout: default `off`**, dogfood by flipping the env level, then change the
   default once trusted. Levels = phases:
   - **Phase 1 `steer`**: SessionStart routing block + WebFetch `deny` + Bash/Grep
     nudges. Measure ctxforge-op uptake on the dashboard (we can now see it).
   - **Phase 2 `wrap`**: Bash transparent wrap on the narrow allowlist (the real
     savings lever). Re-measure.
   - **Phase 3 `full` + Read steering**: add `ctx_execute_file`, enable the Read
     nudge, expand the allowlist, consider `batch_execute` — only where `ops.log`
     data justifies it.
3. **Tool gaps**: build `ctx_execute_file` (small); **defer `ctx_batch_execute`**.
4. **WebFetch: hard `deny` + steer** (clear win, low frequency; easy to dial back).
5. **Bash allowlist: start narrow** (curl/wget, find, cat, ls -R, rg/grep,
   tail/head, build tools, `git log`/`git diff`, test runners), subcommand-aware
   for git, **never** wrap stateful/navigation commands or chains containing them;
   expand from `ops.log` evidence.

---

## 7. Kick off with /goal

```
/goal Read CTXFORGE_ROUTING_PLAN.md and execute the §5 task graph wave by wave, dispatching the wave-2 tasks (T1, T2, T3) to parallel subagents in isolated worktrees, then T4. Build every level behind CTXFORGE_ROUTING (default off = a true no-op). Constraints: re-author original Rust — study but never copy the ELv2 Context Mode source; never wrap stateful Bash commands (cd/export/source or chains containing them); offloaded output must be lossless via the reversible store + ctx_retrieve; the MCP server's stdout stays pure JSON-RPC. Done when: cargo test green and clean build; routing unit + integration tests pass; CTXFORGE_ROUTING=off makes PreToolUse return {} identical to today; with =full a WebFetch payload denies, a wrappable Bash command rewrites to `ctxforge wrap -- …`, and `cd x && y` passes through unchanged; `ctxforge wrap` offloads large output and `ctxforge verify --roundtrip` reproduces it byte-for-byte; a wrapped op appears in ops.log and on the dashboard; SessionStart injects the routing block.
```

This goal is autonomously closable: every clause above is a runnable check, and
the task graph is partitioned for parallel agents.
