# /goal: Build `lens` — a token-saving MCP server in Rust

You are building a complete, working, tested Rust MCP server called **lens** from scratch in this repository. Follow this plan exactly. The end state must compile, pass all tests, and be installable into Claude Code by following the README. Do not stub features — every tool listed must actually work.

---

## 0. Operating instructions for you (Claude Code)

- Build **incrementally in the phase order below**. After each phase, run `cargo build` and `cargo test` and fix everything before moving on. Do not advance with a red build or failing tests.
- Use a TODO list and check off each phase as you complete it.
- Prefer the standard library and a small number of well-known crates (listed in §2). Do not add heavy dependencies not listed without noting why.
- Every module gets unit tests in the same file (`#[cfg(test)]`). Integration tests go in `tests/`. **No feature is "done" until it has passing tests.**
- When you finish, run the full verification checklist in §9 and report the results.
- If something in this plan is ambiguous, make the simplest reasonable choice, write it down in a `DECISIONS.md`, and keep going. Do not stop to ask.

---

## 1. What this is and why

`lens` reduces the tokens an AI coding agent spends per session. It fuses three deterministic primitives borrowed (conceptually, not by copying code) from three open-source projects:

1. **Darkroom** (the "think in code" idea from context-mode): the agent runs a script in a subprocess; only `stdout` returns to context, not the raw data the script processed. This is the single biggest token saver.
2. **Discovery** (the AST-extraction idea from graphify): parse the repo once with tree-sitter into a structural graph of symbols and relationships, so the agent queries a scoped subgraph instead of reading many files.
3. **Compression + reversibility** (the CCR idea from headroom): when a result is large, store the full version locally and return a compact view, retrievable on demand.

**Design principle: the darkroom is ~90% of the value and the cheapest to build. Build it first and make it rock-solid. The other layers are additive but smaller.**

License posture: this is original code. You may study MIT (graphify) and Apache-2.0 (headroom) projects for *ideas* only. Do **not** copy code from the ELv2-licensed context-mode project. This repo is licensed Apache-2.0 (write the LICENSE file).

---

## 2. Tech stack and dependencies

- Language: **Rust** (edition 2021), stable toolchain.
- MCP SDK: **`rmcp`** (the official Rust MCP SDK) with stdio transport. If the exact API differs from this plan, adapt to the installed crate version's real API — the tool *behavior* is what matters, not the exact call shape.
- Crates (pin reasonable recent versions in `Cargo.toml`):
  - `rmcp` — MCP server (stdio transport)
  - `tokio` — async runtime (features: `rt-multi-thread`, `macros`, `process`, `io-util`)
  - `serde`, `serde_json` — serialization
  - `rusqlite` (bundled feature) — SQLite + FTS5 for the index and the retrieval store
  - `tree-sitter` plus grammar crates: `tree-sitter-rust`, `tree-sitter-python`, `tree-sitter-javascript`, `tree-sitter-typescript`, `tree-sitter-go`
  - `anyhow`, `thiserror` — error handling
  - `tracing`, `tracing-subscriber` — logging to stderr (never stdout; stdout is the MCP channel)
  - `blake3` — hashing for content-addressed retrieval keys
  - `walkdir`, `ignore` — repo walking that respects `.gitignore`
  - Dev: `tempfile`, `assert_cmd`, `predicates` — for integration tests

**Critical constraint:** the MCP server speaks JSON-RPC over **stdout**. All logging, diagnostics, and tree-sitter noise must go to **stderr**. A single stray `println!` will corrupt the protocol. Enforce this.

---

## 3. Repository layout

Create exactly this structure:

```
lens/
├── Cargo.toml
├── README.md                # install + usage (see §8)
├── LICENSE                  # Apache-2.0 full text
├── DECISIONS.md             # any choices you made under ambiguity
├── .gitignore
├── rust-toolchain.toml      # pin stable
├── src/
│   ├── main.rs              # entrypoint: build server, register tools, run stdio
│   ├── server.rs            # MCP server wiring + tool dispatch
│   ├── darkroom/
│   │   ├── mod.rs           # lens_run: run code in subprocess, capture stdout
│   │   └── runtimes.rs      # language → interpreter/command mapping
│   ├── index/
│   │   ├── mod.rs           # lens_index / lens_search over SQLite FTS5
│   │   └── schema.rs        # FTS5 table creation + migrations
│   ├── discovery/
│   │   ├── mod.rs           # repo walk + dispatch to per-language extractors
│   │   ├── graph.rs         # Graph data model (nodes, edges) + (de)serialization
│   │   ├── extract.rs       # tree-sitter parsing → symbols + relationships
│   │   └── query.rs         # lens_symbol / lens_links / lens_path traversal
│   ├── store/
│   │   ├── mod.rs           # reversible store: put full blob, return compact ref
│   │   └── compress.rs      # structural JSON compaction (deterministic)
│   └── tools.rs             # tool input/output structs (serde) shared across modules
└── tests/
    ├── darkroom_tests.rs
    ├── index_tests.rs
    ├── discovery_tests.rs
    ├── store_tests.rs
    └── e2e_tests.rs         # spawn the binary, speak MCP, assert tool results
```

---

## 4. Data directory

All persistent state lives under a per-repo directory: `./.lens/` in the working repo (add it to `.gitignore`). Layout:

- `.lens/index.db` — SQLite FTS5 index (for `lens_index`/`lens_search`)
- `.lens/graph.json` — the discovered structural graph
- `.lens/store.db` — SQLite reversible store (full blobs keyed by blake3 hash)

Allow override via env var `LENS_DIR`. Create the directory on first use.

---

## 5. The MCP tools (this is the contract — implement all of them)

Each tool takes a JSON object and returns a JSON result. Define request/response structs in `tools.rs` with serde. Validate inputs; return structured errors, never panic across the MCP boundary.

### 5.1 `lens_run` — the darkroom (BUILD FIRST, highest priority)
**Input:** `{ "language": "python|javascript|typescript|bash|ruby|go", "code": "<source>", "timeout_secs": <int, default 30>, "stdin": "<optional>" }`
**Behavior:** Write the code to a temp file (or pipe via stdin per runtime), spawn the matching interpreter in a subprocess in the repo's working dir, enforce the timeout (kill on overrun), capture stdout and stderr separately.
**Output:** `{ "stdout": "<captured>", "stderr": "<captured>", "exit_code": <int>, "timed_out": <bool>, "stdout_bytes": <int> }`
**The point:** the *raw data the script reads never enters context* — only what the script prints. This is the core value. Make it reliable: handle missing interpreters gracefully (return a clear error telling the user to install it), never hang (timeout always fires), never leak temp files.
**Large-output handling:** if `stdout` exceeds `LENS_MAX_INLINE` bytes (default 8 KB), store the full stdout in the reversible store (§5.6) and return a truncated head + tail plus a `retrieve_ref`. The agent can call `lens_recall` to get the rest.

### 5.2 `lens_index` — index content into FTS5
**Input:** `{ "path": "<file or dir>", "recursive": <bool, default true> }`
**Behavior:** Walk the path (respect `.gitignore` via the `ignore` crate), chunk text/markdown/code files by reasonable boundaries (headings for markdown; ~100-line windows for code), insert chunks into an FTS5 table with `(path, chunk_id, content)`. Use porter tokenizer.
**Output:** `{ "files_indexed": <int>, "chunks": <int> }`

### 5.3 `lens_search` — query the FTS5 index
**Input:** `{ "queries": ["<q1>", "<q2>"], "limit_per_query": <int, default 5> }`
**Behavior:** Run BM25-ranked FTS5 MATCH for each query, return top snippets with path + a window around the match. Support multiple queries in one call (saves round-trips).
**Output:** `{ "results": [ { "query": "...", "hits": [ { "path": "...", "snippet": "...", "score": <f64> } ] } ] }`

### 5.4 `lens_map` — build the structural graph (one-time / on demand)
**Input:** `{ "path": "<repo root, default '.'>", "languages": ["rust","python",...] (optional filter) }`
**Behavior:** Walk the repo (respect `.gitignore`), and for each supported file, parse with the matching tree-sitter grammar. Extract **nodes** (functions, methods, structs/classes, modules, with file + line) and **edges** (calls, imports, defines/contains, type references — extract what each grammar reasonably supports; it's fine to start with definitions + calls + imports). Write the graph to `.lens/graph.json`. This is deterministic, local, no LLM, no API calls.
**Output:** `{ "nodes": <int>, "edges": <int>, "files_parsed": <int>, "languages": ["..."] }`
**Scope:** support **Rust, Python, JavaScript, TypeScript, Go** in v1. Structure the extractor so adding a language is a small, isolated change (a trait impl + a query). Document how in `DECISIONS.md`.

### 5.5 `lens_symbol` / `lens_links` / `lens_path` — traverse the graph
- `lens_symbol` — **Input:** `{ "name": "<symbol substring>", "kind": "<optional: function|struct|...>", "limit": <int> }` → returns matching nodes with file/line and immediate connections. Compress the returned subgraph through §5.6 if large.
- `lens_links` — **Input:** `{ "node_id": "<id>", "depth": <int, default 1> }` → returns the local subgraph around a node.
- `lens_path` — **Input:** `{ "from": "<node id or name>", "to": "<node id or name>" }` → shortest path between two symbols (BFS over edges).
**The point:** the agent answers "what calls X / how does A reach B" with a small subgraph instead of reading many files.

### 5.6 `lens_recall` — reversible store lookup
**Input:** `{ "ref": "<retrieve_ref returned by another tool>" }`
**Output:** `{ "content": "<the full stored blob>" }`
**Behavior:** Look up the blake3-keyed blob in `store.db` and return it. This is the "reversibility" guarantee: nothing compressed or truncated is ever truly lost — the agent can always pull the full version on demand.

### 5.7 `ctx_stats` — savings + diagnostics
**Input:** `{}`
**Output:** `{ "darkroom_calls": <int>, "raw_bytes_processed": <int>, "bytes_returned_to_context": <int>, "estimated_tokens_saved": <int>, "index_chunks": <int>, "graph_nodes": <int>, "graph_edges": <int> }`
**Behavior:** Track per-session counters (in `store.db` or a small stats table). `estimated_tokens_saved ≈ (raw_bytes_processed - bytes_returned_to_context) / 4`. This is how the user *measures the residual* and decides whether the heavier layers are worth it.

---

## 6. Build phases (do them in this order)

### Phase 1 — Skeleton + darkroom (the 90%)
- `cargo init`, set up `Cargo.toml`, `main.rs`, `server.rs` with `rmcp` stdio server that registers and dispatches tools.
- Implement `lens_run` fully (§5.1) including timeout, stdout/stderr capture, large-output → store fallback.
- Implement the reversible store (§5.6) and `lens_recall` — the darkroom needs it for large output.
- Implement `ctx_stats` counters for darkroom usage.
- **Tests:** darkroom runs python/bash, captures stdout only, enforces timeout (a sleep that overruns is killed and `timed_out: true`), handles a missing interpreter cleanly, large output is stored and retrievable by ref. Verify a script that reads a big file but prints one line returns only that line (the core invariant).
- **Gate:** `cargo build && cargo test` green before Phase 2.

### Phase 2 — FTS5 index + search
- Implement `schema.rs` (FTS5 virtual table, porter tokenizer, migrations) and `lens_index` / `lens_search`.
- Respect `.gitignore` in the walk.
- **Tests:** index a temp dir of files; search returns the right file with a snippet; multiple queries in one call work; BM25 ordering is sane (an exact-term doc ranks above a weak match).
- **Gate:** green build + tests.

### Phase 3 — Discovery (tree-sitter graph)
- Implement `graph.rs` (model + serde), `extract.rs` (per-language tree-sitter extraction), `discovery/mod.rs` (walk + dispatch), `lens_map`.
- Start with **Rust** end-to-end (extract functions, structs, calls, `use` imports), then add Python, JS, TS, Go. Each language: parse, walk tree, emit nodes + edges.
- Implement `query.rs` with `lens_symbol`, `lens_links`, `lens_path` (BFS).
- Compress large subgraph results through the store.
- **Tests:** for each language, a fixture file produces the expected node kinds and at least the obvious call/import edges; `lens_path` finds a path between two connected symbols and returns none between disconnected ones; `lens_links` depth works.
- **Gate:** green build + tests.

### Phase 4 — Compression + polish
- Implement `compress.rs`: deterministic structural JSON compaction (e.g., dictionary-encode repeated keys/strings, drop nulls, stable shortening) used for large graph/search results, always paired with a store ref so it's reversible.
- Wire `ctx_stats` to include index + graph figures.
- Ensure all logging is on stderr; scrub any stray stdout writes.
- **Tests:** compaction round-trips (compact → ref → retrieve → original), and a large `lens_symbol` returns compact form + working ref.
- **Gate:** green build + tests.

### Phase 5 — E2E + docs
- `tests/e2e_tests.rs`: build the binary with `assert_cmd`, launch it, perform an MCP `initialize` handshake over stdio, then call `lens_run`, `lens_index`+`lens_search`, `lens_map`+`lens_symbol`, and `ctx_stats`, asserting well-formed results. This proves the whole server works as a real MCP server, not just unit-level.
- Write the README (§8) and LICENSE and DECISIONS.md.
- Run §9 verification and report.

---

## 7. Correctness and robustness requirements

- **No panics across the MCP boundary.** Every tool handler returns `Result`; map errors to structured MCP error responses with helpful messages.
- **Timeouts always fire.** A runaway script must be killed and reported, never hang the server.
- **stdout is sacred.** Only JSON-RPC goes to stdout. Logging → stderr. Add a comment in `main.rs` stating this.
- **Deterministic discovery.** Same repo in → same graph out (stable node IDs: hash of `file:kind:name:line`). Tests depend on this.
- **Respect `.gitignore`** everywhere you walk a repo.
- **Graceful missing interpreters/grammars.** If a language's runtime isn't installed, `lens_run` returns a clear "install X" error; if a grammar fails to parse a file, skip it and record a warning in the result, don't abort the whole discovery.
- **Concurrency-safe SQLite.** Open connections per-operation or use a mutex; don't share a raw connection across threads unsafely.

---

## 8. README.md (write this file completely)

The README must let someone go from zero to a working tool in Claude Code. Include:

1. **What it is** — one paragraph: a Rust MCP server that cuts agent token use via darkroom execution, FTS5 search, a tree-sitter code graph, and reversible compression.
2. **Prerequisites** — Rust stable (`rustup`), and the optional language runtimes used by `lens_run` (python3, node, bash, etc.) with a note that each is only needed if you run that language.
3. **Build** —
   ```
   git clone <repo> && cd lens
   cargo build --release
   ```
   Binary at `target/release/lens`.
4. **Install into Claude Code** — exact command:
   ```
   claude mcp add lens -- /absolute/path/to/target/release/lens
   ```
   …and the equivalent manual `.mcp.json` / `claude_desktop_config.json` block:
   ```json
   {
     "mcpServers": {
       "lens": {
         "command": "/absolute/path/to/target/release/lens"
       }
     }
   }
   ```
   Note that after adding, the user restarts Claude Code and verifies with a quick `ctx_stats` call.
5. **Tool reference** — a table of all tools from §5 with one-line descriptions and example inputs.
6. **Recommended workflow** — the staged philosophy: run `lens_map` once per repo, then lean on `lens_run` for everything data-heavy, use `lens_search` for lookup, `lens_symbol` for structure, and check `ctx_stats` to see savings. Note that the darkroom is where most savings come from.
7. **Configuration** — env vars: `LENS_DIR`, `LENS_MAX_INLINE`.
8. **Development** — `cargo test` runs everything; how to add a language (point to DECISIONS.md).
9. **License** — Apache-2.0.

---

## 9. Final verification checklist (run and report all of these)

- [ ] `cargo build --release` succeeds with no warnings (fix or justify any).
- [ ] `cargo test` — every test passes; report the count.
- [ ] `cargo clippy -- -D warnings` is clean (or document exceptions).
- [ ] The binary launches and completes an MCP `initialize` handshake (the e2e test proves this).
- [ ] `lens_run` invariant demonstrated by a test: a script reads a large input but prints one line → only that line returns, raw input never appears in the result.
- [ ] `lens_run` timeout test: an overrunning script is killed, `timed_out: true`.
- [ ] `lens_index` + `lens_search` round-trip on a temp corpus returns the right file.
- [ ] `lens_map` on this repo's own `src/` produces a non-empty graph; `lens_symbol` finds a known function; `lens_path` connects two known-connected symbols.
- [ ] Large output / large subgraph → compact result + working `lens_recall` ref (reversibility proven).
- [ ] `ctx_stats` reports non-zero `estimated_tokens_saved` after a darkroom run.
- [ ] README install steps are accurate against the actual binary path and the real `rmcp` API you built.
- [ ] No `println!`/stdout writes anywhere except the MCP transport (grep to confirm).

When all boxes are checked, print a summary: test count, the `ctx_stats` output from a sample run, and any decisions recorded in DECISIONS.md.

---

## 10. Definition of done

A reviewer can clone the repo, run `cargo build --release` and `cargo test` (all green), follow the README to add `lens` to Claude Code, restart, and successfully call every tool in §5 — with `lens_run` demonstrably returning only script output (not raw data), the graph queryable after one `lens_map`, and `ctx_stats` showing measured savings. Build it so this is true after this single prompt.
