# Decisions

Choices made under ambiguity while building `lens`, per the plan's
instruction to pick the simplest reasonable option and record it.

## MCP SDK (rmcp 1.7)

- Used `rmcp` 1.7 with the `#[tool_router]` / `#[tool]` / `#[tool_handler]`
  macros. Tools take `Parameters<T>` and return `Result<Json<T>, ErrorData>`;
  `Json<T>` populates both the result `content` (text JSON) and
  `structured_content`, so clients that read either field work.
- `schemars` is pinned to **1.0** because rmcp 1.7 depends on it (not 0.8 as the
  plan suggested). The plan explicitly allows adapting to the real crate API.
- `get_info()` is implemented manually to advertise tool capability and server
  instructions; when present, the macro does not generate its own.
- The `client` + `transport-child-process` rmcp features are dev-only
  dependencies, used by the e2e harness so the release binary stays minimal.

## Darkroom (`lens_run`)

- Runtimes: python→`python3`, javascript→`node`, typescript→`npx --yes tsx`,
  bash→`bash`, ruby→`ruby`, go→`go run`. TypeScript uses `tsx` so no build step
  is required; it is fetched on first use via `npx`.
- Timeout is enforced by taking the stdout/stderr pipes into reader tasks and
  awaiting `child.wait()` under `tokio::time::timeout`; on overrun the child is
  killed and `timed_out: true` is returned with `exit_code: -1`.
- Temp scripts use `tempfile` (auto-deleted on drop) so nothing leaks.
- Large stdout (> `LENS_MAX_INLINE`, default 8 KB) is offloaded to the
  reversible store and replaced with a head+tail preview plus a `retrieve_ref`.

## Stats metric

- `raw_bytes_processed` counts the **full** stdout each script produced;
  `bytes_returned_to_context` counts what was actually returned inline (preview +
  stderr). `estimated_tokens_saved = max(0, raw - returned) / 4`.
- We cannot observe how much data a script *read* (that is the whole point —
  it never enters context), so savings are measured at the output boundary and
  materialise when large output is offloaded rather than inlined.

## Index (`lens_index` / `lens_search`)

- Single FTS5 virtual table `chunks(path UNINDEXED, chunk_id UNINDEXED, content)`
  with the `porter` tokenizer. Markdown is chunked by headings; other files by
  ~100-line windows.
- Re-indexing a path deletes its existing rows first, so indexing is idempotent.
- Queries are sanitised into quoted tokens before `MATCH` to avoid FTS5 syntax
  errors from punctuation. Ranking is BM25; scores are returned negated so that
  higher = better.

## Discovery (`lens_map` + graph)

- Stable node IDs are `blake3(file:kind:name:line)` truncated to 16 hex chars.
  Files are processed in sorted order and nodes/edges sorted before saving, so
  the same repo always produces byte-identical `graph.json`.
- Edge kinds: `contains` (module→definition), `calls` (resolved by callee name
  within the repo), `imports` (module→matching repo symbol, or a synthetic
  `import` node when the target is external).
- `lens_path` traverses **only** semantic edges (`calls`/`imports`), excluding
  `contains`, so two unrelated functions in the same file are correctly reported
  as having no path. `lens_links` includes all edges.
- Call attribution walks up the AST from each call site to the nearest enclosing
  callable scope; top-level calls are attributed to the file's module node.

### Adding a language

Discovery is structured so a new language is an isolated change in
`src/discovery/extract.rs`:

1. Add a `LangSpec` to `all_specs()` with: `name`, file `extensions`, a
   `language` closure returning the tree-sitter `Language`, and three queries —
   `defs_query` (each capture name becomes the symbol `kind`), `calls_query`
   (one `@call` capture on the callee name), and `imports_query` (one capture on
   the import statement).
2. Add an entry to `fn_scope_kinds()` listing the AST node kinds that delimit a
   callable scope, used to attribute calls to their enclosing definition.
3. Add the grammar crate to `Cargo.toml`.

No other module changes are required.

### Provenance (Graphify, MIT)

The *idea* — parse a repo once into a queryable graph of symbols and
relationships so the agent reasons over a scoped subgraph instead of reading
many files — is from **Graphify** (MIT). It was the named lineage source for
this layer in the build plan (`LENS_PLAN.md` §2, `LENS_ROUTING_PLAN.md`),
studied for the *concept only* under the plan's explicit "study MIT/Apache
projects for ideas, do not copy code" posture — via design notes on its
cache layout and edge model, not its source tree. The implementation is
written fresh in Rust on the `tree-sitter` crate: the per-language `LangSpec`
queries (`extract.rs`), the `blake3(file:kind:name:line)` node IDs, the
`calls`/`imports`/`contains` edge model, and the BFS `neighbors`/`shortest_path`
(`graph.rs`) are all original. No Graphify source is vendored or copied.
**Left behind:** any LLM/semantic-enrichment layer — lens's graph is purely
deterministic tree-sitter extraction.

## Compression (`compress.rs`)

- Deterministic, reversible JSON compaction: drop null object fields, then
  dictionary-encode repeated string values (length ≥ 5, appearing ≥ 2×) into a
  shared table, replacing each with `{"$": index}`. Output is
  `{"_d": [...], "_v": ...}`; `expand_json` is the exact inverse.
- Large `lens_symbol` / `lens_links` results are stored in full (plain
  JSON) for `lens_recall` and returned in compacted form — reversibility is
  guaranteed by the store ref regardless of the compaction.

## SQLite concurrency

- Both the store and index open a fresh connection per operation with a busy
  timeout, rather than sharing one connection across async tasks. Simple and
  safe for a single-client stdio server.

## Data directory

- State lives in `./.lens/` (`index.db`, `store.db`, `graph.json`),
  overridable via `LENS_DIR`, created on first use.

## Session continuity (`lens hook` / `lens session`)

### Claude Code hook contract adapted to

Bound against the installed Claude Code version (cross-checked against Context
Mode's wiring at `~/.claude/plugins/cache/context-mode/context-mode/<v>/hooks/`).
The real contract, as implemented:

- **stdin** is a single JSON object. Fields used: `session_id`,
  `transcript_path`, `cwd` (project dir), `source` (SessionStart:
  `startup|compact|resume|clear`), `prompt` (UserPromptSubmit),
  `tool_name` / `tool_input` / `tool_response` (Pre/PostToolUse). `trigger`
  (PreCompact) is accepted but unused.
- **session id** resolution: `transcript_path` file stem (the UUID) → `session_id`
  → `pid-<pid>` fallback. **project dir**: `cwd` → `CLAUDE_PROJECT_DIR` env →
  process cwd. (Matches Context Mode's `getSessionId` / `getInputProjectDir`.)
- **stdout** is the response channel:
  - `SessionStart` →
    `{"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"…"}}`.
    The `additionalContext` string is injected into the resumed model context.
  - All other events → `{}` (an empty object = no decision = allow). PreToolUse
    is capture-only; it never blocks.
- Hooks are short-lived separate processes, so their stdout is theirs to use
  (unlike the MCP server, whose stdout is the JSON-RPC channel). Logging stays on
  stderr; every error is swallowed so a hook can never block the session.
- Registration lives in `~/.claude/settings.json` under `hooks.<Event>[]` as
  `{matcher, hooks:[{type:"command", command}]}`. `lens session install`
  embeds the **absolute** binary path (`current_exe()`) so hooks fire regardless
  of PATH. Overridable for tests via `LENS_SETTINGS`.

### Conflict guard (what makes the swap atomic)

`install` refuses if Context Mode is present: either an enabled `context-mode*`
key under `enabledPlugins`, or any hook `command` string mentioning
`context-mode`. Two systems on the same lifecycle events corrupt each other's
state, so this is a hard refusal, not a warning.

### Snapshot = guide (one artifact)

§2.2 (snapshot) and §2.3 (restore guide) are the same artifact: PreCompact builds
the budget-bounded Session Guide and persists it; SessionStart re-emits it. The
budget (`LENS_SNAPSHOT_BUDGET`, default 2048 B) drops optional tiers
lowest-priority-first while always preserving the must-keep set (last request,
tasks, project rules, files modified, unresolved errors, key decisions). Per-item
caps keep the must-keep set small (rule **paths** only inline; full rule content
goes to the FTS5 index for `lens_search`).

### Borrowed-as-pattern vs written-fresh

- **Pattern-borrowed from Context Mode** (studied, not copied): the hook I/O
  contract above; the event taxonomy and P1–P4 prioritization; the lifecycle
  semantics (fresh `startup` = clean slate; `compact`/`resume` rehydrate; the
  `/resume`-gives-a-fresh-id → claim-latest-unconsumed-snapshot fallback);
  capturing `CLAUDE.md`/`AGENTS.md` at startup because they load as system
  context invisible to PostToolUse.
- **Written fresh** (original Rust): the entire implementation — the SQLite event
  store (`session.db`: `session_events` / `session_meta` / `session_resume`), the
  extractors (`extract_tool_events` / `extract_user_events`), the budget-tiered
  snapshot builder, the restore path, the `lens hook` dispatcher, and the
  settings.json install/uninstall/status with the conflict guard. lens inlines
  the working state into the guide; Context Mode defers most detail to an events
  file + `lens_search`.

### Recovery benchmark finding (mock oracle)

The three-arm recovery benchmark (`bench_recovery`) drives the same scenario
stream through the no-continuity floor, Context Mode's **real** hook scripts (via
`bun`), and lens's pipeline. Under the context-presence mock oracle, lens
≥ Context Mode on both scenario sets (100% vs 75%), with the genuine wins being
that Context Mode's injected snapshot **drops `TodoWrite` tasks and inline tool
errors** (it keeps only intent/role/decisions/file-basenames), where lens
surfaces tasks and unresolved errors directly. To avoid overclaiming on
formatting, file-recovery evidence uses the **basename** both systems capture
(lens additionally keeps full paths). Real-model numbers require
`ANTHROPIC_API_KEY`; the Context Mode arm is reported as `n/a` if `bun`/the plugin
are not runnable rather than faked.

## Savings: columnar compaction (faithful SmartCrusher port)

The first savings run had three weak workloads. The §0.1 scale-curve diagnostic
(`cargo run --bin bench_scale_curve`, drives the **real** lens code path at
1×/10×/50× the committed fixture) classified them:

| Workload | 1× | 10× | 50× | Verdict |
| --- | --- | --- | --- | --- |
| Code search (index) | 37% | 94% | 99% | **artifact** — rises sharply with size; the 12-file fixture was the problem, the mechanism is fine. |
| Issue triage (compression) | 33% | 37% | 37% | **real weakness** — flat at scale. The compactor was a naive approximation. |
| Codebase exploration (discovery) | 20% | 18%* | 0%* | small-fixture artifact at 1×; *scaled numbers are a pessimistic lower bound under a duplicate-symbol replication, see BENCHMARKS.md. |

Only **issue triage stayed weak at scale**, so per §0.5 we read the real
reference algorithm and diffed it against ours.

### Reference read (the actual code, not the README)

Headroom's SmartCrusher Python is now a thin PyO3 shim; the real algorithm is in
`crates/headroom-core/src/transforms/smart_crusher/compaction/` (`compactor.rs`,
`formatter.rs`, `ir.rs`). The lossless mechanism is **CSV-schema compaction**:
detect a homogeneous array of objects, emit the column schema **once**
(`[N]{col:type,col:type?,…}`), then each row as a value-only tuple. Its own
formatter test comment confirms the dominant byte win is schema dedup, not the
CSV punctuation removal ("the win comes from removing structural punctuation
only — modest, but real"). `lossless_min_savings_ratio = 0.30` and the
`markdown-kv` note ("KV repeats field names per row, so it clears the gate less
often than CSV") both point at the same thing: **per-row key repetition is the
cost, factoring the schema out is the win.**

### Diff: what the real algorithm had that ours did not

lens's `compress::compact_json` did only (1) drop nulls and (2)
dictionary-encode repeated string *values*. Missing vs SmartCrusher:

- **Schema/template extraction (columnar transposition)** — emit field names
  once, rows as positional tuples. ← **PORTED.** This is the whole gap: an array
  of 24 homogeneous 10-field issues repeated every key 24×. `transpose()` now
  replaces any array of ≥2 objects with an identical key set by
  `{"_tbl":{"c":[cols],"r":[[vals],…]}}`, recursively. Exact inverse
  `untranspose()` keeps `expand(compact(v)) == drop_nulls(v)`.
- **Dictionary-encode repeated *values*, not just keys** — ← already present;
  kept, and now runs over the transposed form so repeated categorical column
  values (status/component/assignee) still collapse.
- **Drop null/empty fields** — ← already present; kept.
- **Nested-uniform flatten to dotted columns, stringified-JSON recursion,
  heterogeneous bucket-by-discriminator** — ← **not ported.** They help shapes
  the lens payloads (issue lists, graph node/edge sets) don't exhibit: those
  are already flat homogeneous tables. Recursion into nested homogeneous arrays
  *is* handled (the graph `{nodes,edges}` case). Deferred as speculative until a
  real payload needs them.
- **Opaque-blob → CCR pointer** — ← not ported into `compact_json` itself;
  lens already offloads any oversized payload to the reversible store with a
  `retrieve_ref` at the tool boundary (`Forge::maybe_compact`, darkroom/index
  offload), which is the same CCR idea applied one layer up.
- **Lossy row-sampling / anomaly preservation (keep ~50 of 1000 rows)** — ←
  **deliberately not ported.** It is lossy and not reversible. lens keeps
  every row; the residual (long unique issue *bodies* — genuine prose) is what
  CCR/the store is for, not a denser deterministic encoding. Per the hard
  constraint: deterministic only, no ML / Kompress prose model.

### Result

Issue triage `8,902 → 3,885 bytes = 56%` (was 33%) on the committed fixture, and
56→61% across the scale curve (was flat 37%). The residual is unique prose
bodies, which no deterministic codec shrinks — an honest 56–61%, not forced
higher. The columnar form is reversible by exact inverse **and** by the store
ref, so `Forge::maybe_compact` (which compacts large graph subgraphs) inherits
the win — that is why codebase-exploration 10× recovered from 0% to 18% with no
discovery-path change.

### Reversibility guard

`transpose` is skipped wholesale when the input already contains a `_tbl`-keyed
object (`contains_key`), with the decision recorded as `"_t": false` in the
output so `expand` never mis-rebuilds a user's literal into an array. The store
always holds the untouched original regardless.

## Accuracy: real-model run via `claude-pty`, and the discovery-slip finding

The real-model accuracy/recovery arms call a model. The committed account's
**API credit balance is zero**, so the `curl` Anthropic path returns
`credit balance too low`. Rather than leave accuracy permanently "pending", the
harnesses gained a third backend, `Model::ClaudePty`, selected with
`LENS_BENCH_BACKEND=claude-pty`: it drives interactive Claude Code through
the `claude-pty` binary, which bills against **plan quota** instead of the Agent
SDK credit pool. Tools are disabled (`--allowed-tools ""`) and it runs in the
already-trusted project dir, so each arm answers purely from its given context —
the same isolation as the API arm — with no `--dangerously-skip-permissions`.
The screen scrape echoes the prompt, so the answer is recovered as the **last
balanced `{…}`** object (`last_json_object`); `--session-id` transcript mode was
tried first but couldn't detect end-of-turn and hard-timed-out.

**The discovery-slip finding (worked example of reading a negative delta).** The
first real run, on `claude-haiku-4-5`, showed discovery **−33pp** (N=3, so one
task = 33pp). The auto-warning flags any negative delta as "dropping load-bearing
context", but investigation showed otherwise: the one regressing task
(`0008_reachable_path`, "can `handle_request` reach `connect_db`?") has a
treatment context — the `lens_path` op — that returns the **correct** answer
(`found:true`, full path `handle_request → fetch_user → connect_db`), i.e. *more*
explicit than the raw-file control, yet Haiku still answered `reachable:false`.
A weak-model reasoning slip on a correct context, **not** a lens context-drop.
Re-running just the discovery set on `claude-sonnet-4-6` (via the same backend)
confirmed it: discovery returns to **100% / 100% (+0pp)**, `0008` answering
`reachable:yes`. Lesson encoded in `bench_report`: a negative aggregate delta is
necessary-not-sufficient evidence of a context-drop; per-task + cross-model
checks separate a real regression from model noise before the table is trusted.
