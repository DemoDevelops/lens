# /goal: Add an observability layer to `lens` (see under the hood at runtime)

lens's correctness is proven on *fixtures* (the benchmark suite). What's missing is *runtime visibility* — when lens runs live inside a real Claude Code session (or many parallel agents), there is currently no way to see what it actually did. This goal adds observability so running lens becomes something you can **watch and verify**, not something you trust on faith. The reversible store already guarantees nothing is lost; this surfaces that guarantee and the live behavior.

**Read first:** the existing `src/` — the tool handlers (`lens_run`, `lens_search`, `lens_symbol`, etc.), `ctx_stats` and its counters, the reversible store (`store/`), and how logging is currently wired (stderr). Match existing conventions. This is additive instrumentation; do **not** change tool behavior or measured outputs.

---

## 0. Principles

- **Additive only.** No tool's result payload changes. Observability is a side channel (log files + read-only query commands + opt-in debug fields), never a modification of what the agent receives.
- **stdout stays sacred.** The MCP server's stdout is JSON-RPC only. All observability output goes to **log files** and **separate CLI subcommands**, never to the server's stdout.
- **Cheap when off, rich when on.** Logging is always-on but lightweight (append-only). The verbose "explain" detail is opt-in via env/flag so normal runs aren't slowed.
- **Concurrency-safe.** Multiple agents may hit lens at once (parallel fan-out). The op log and stores must tolerate concurrent writers without corruption or lock stalls (see §4).

---

## 1. Persistent operation log

Every tool invocation appends one structured record to `.lens/ops.log` (JSONL; path honors `LENS_DIR`).

Each record:
```json
{
  "ts": "<iso8601>",
  "session_id": "<if known>",
  "agent_id": "<if distinguishable in parallel runs>",
  "tool": "lens_run|lens_search|lens_symbol|...",
  "input_summary": { "language": "python", "code_bytes": 812 },   // NOT full input — a summary
  "raw_bytes_in": 51234,        // bytes the op processed (e.g. file read inside darkroom)
  "bytes_returned": 612,        // bytes actually returned to context
  "tokens_saved_est": 12655,    // (raw_bytes_in - bytes_returned)/4
  "store_ref": "<blake3 ref if a full blob was stored>",
  "duration_ms": 41,
  "outcome": "ok|error|timed_out",
  "note": "<short, e.g. 'large stdout stored, head+tail returned'>"
}
```

- Append-only, one line per op. Never rewrites. Rotates at a size cap (`LENS_OPS_LOG_MAX`, default ~10 MB) to `.lens/ops.log.1`.
- This is the always-on record. A live session now has a tailable file: `tail -f .lens/ops.log`.
- **Do not log full inputs/outputs** in the always-on log (privacy + size) — summaries + the store_ref. The full data is already in the reversible store, retrievable on demand.

---

## 2. Live stats view (`lens stats`)

A CLI subcommand (not an MCP tool) that reads the counters + ops log and prints a human view:

- `lens stats` — one-shot summary: total ops, by tool; cumulative raw_bytes_in, bytes_returned, tokens_saved_est; per-mechanism breakdown (darkroom/index/compression/discovery); error/timeout counts; store size; index chunk count; graph node/edge count.
- `lens stats --watch` — refreshes every ~1s (re-reads the counters), so during a live session you watch savings accumulate in real time.
- `lens stats --session <id>` — scope to one session.
- `lens stats --since <ts>` / `--last <n>` — windowed.

Output is plain text, aligned, no color dependency required. This turns "I'm trusting it saved tokens" into "I can see the running total."

---

## 3. Verify / replay (`lens verify`) — the reversibility audit

The scariest failure for any compressor/darkroom is "did it silently drop something I needed." The store already guarantees losslessness; this lets you **see** it held.

- `lens verify <store_ref>` — re-fetch the full blob for a ref and print it (so you can inspect what the compact result was standing in for).
- `lens verify --op <ops.log line / op id>` — for a logged op that produced a compact result + ref, re-expand from the store and show: the compact form the agent saw, the full form, and a confirmation that the recorded `bytes_returned`/`store_ref` are consistent.
- `lens verify --roundtrip <store_ref>` — for compaction ops, run the exact inverse (untranspose/decompact) and assert it reproduces the original byte-for-byte; print PASS/FAIL. This is the lossless contract, checkable on **real session data**, not just unit fixtures.
- `lens verify --all-recent <n>` — roundtrip-check the last N store-backed ops, report any that don't reproduce exactly. (Should always be all-PASS; if not, that's a real bug surfaced.)

---

## 4. Concurrency hardening (the parallel-agent regime the benchmark skipped)

Real fan-out (many agents at once) is a regime the single-session benchmark never tested. Make observability and stores safe under it, and add a way to *see* contention.

- **SQLite under concurrent writers:** ensure WAL mode is enabled on all lens SQLite DBs (index, store, stats, session) so concurrent readers/writers don't block each other; use busy-timeout so writers retry rather than erroring with "database is locked". Per-operation connections as already done.
- **Ops log concurrency:** appends from parallel processes must not interleave-corrupt lines — use atomic append (single `write` of a complete line, or per-process buffered flush of whole records). Tag each record with `agent_id`/pid so a parallel run is disentangle-able.
- **Contention visibility:** record `lock_wait_ms` (time spent waiting on a busy DB) in the op record when non-zero, so `lens stats` can show whether concurrency is causing stalls. This is how you'd *see* the parallel-agent risk rather than guess at it.
- **Add a concurrency stress test (§6):** spawn N parallel workers all driving lens ops against shared stores; assert no corruption, no lock errors, and roundtrip-verify all results. This is the regime the benchmark suite skipped — close it.

---

## 5. Debug / explain mode (opt-in, per-op detail)

When `LENS_EXPLAIN=1` (or a `--explain` server flag), lens additionally writes a verbose per-op trace to `.lens/explain.log`:

- The full decision trail for an op: input shape, which path/branch was taken (e.g. "stdout 47KB > inline cap 8KB → stored ref X, returned head+tail"), compaction technique applied, sizes at each stage.
- This is the "open it up and watch one operation think" view, for when something feels off.
- Off by default (verbose + larger). Never affects the agent-facing result — explain detail goes to its own log, not into the tool response.

---

## 6. Tests / verification

- Every tool invocation writes exactly one well-formed ops.log record with correct byte/token fields (assert against a known op).
- `lens stats` totals reconcile with the sum of ops.log records.
- `lens verify --roundtrip` PASSes on real store-backed compaction ops; a deliberately corrupted store entry makes it FAIL (proves the check is real, not a rubber stamp).
- **Concurrency stress test:** N parallel workers, shared stores, WAL on — zero corruption, zero unhandled lock errors, all results roundtrip-verify, and any `lock_wait_ms` is surfaced in stats.
- Ops log rotation at the size cap works; stdout of the MCP server remains pure JSON-RPC (grep-confirm no observability output leaked to stdout).
- `LENS_EXPLAIN=1` produces explain.log without changing any tool result payload (assert result bytes identical with and without explain on).
- `cargo test` green; clean build.

## 7. Definition of done

Running lens is now observable: a tailable `ops.log` records every operation with byte/token/duration/outcome; `lens stats [--watch]` shows live cumulative savings and per-mechanism breakdown; `lens verify` re-expands and roundtrip-checks real results from the store so the lossless guarantee is auditable on live data, not just fixtures; WAL + busy-timeout + a passing concurrency stress test make the parallel-agent regime safe and its contention visible; and `LENS_EXPLAIN=1` gives a per-op decision trace. No tool result payload changed; MCP stdout stays pure. The tool you were trusting on faith is now one you can watch and verify in real time.
