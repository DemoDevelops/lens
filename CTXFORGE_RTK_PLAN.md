# ctxforge ⇄ RTK integration plan — adopt the headroom pattern

> Execute with `/goal` (see §7). Wave-2 tasks dispatch to parallel sub-agents in
> isolated git worktrees. **Standing rule for this repo: read the actual RTK /
> headroom source before implementing each task — never guess. The relevant
> source is cited inline below.**

## 0. Problem

ctxforge's own Bash routing (`CTXFORGE_ROUTING` wrap) fires ~never on real
workloads: every command Claude Code issues is `cd "<proj>" && …`, and ctxforge's
`is_stateful` rejects any chain containing `cd`. Measured live in the Meridian
session: 782 built-in tool events, **0** ctxforge ops. Meanwhile the user's prior
stack (RTK) proxied **3,753** commands on the same machine (`~/Library/Application
Support/rtk/history.db`) because RTK rewrites *per segment* (`cd x && grep` →
`cd x && rtk grep`) and ships per-command compactors.

Re-authoring RTK is the wrong move: RTK is **2.46 MB of Rust, a binary crate with
no `lib` target** (so not a cargo dependency), with compactors of 600–2,800 lines
each. Headroom already solved this: it does **not** port RTK — it ships the RTK
binary and reads its savings. We do the same.

## 1. Approach — the headroom division of labor (verified)

From `chopratejas/headroom` `docs/rtk-architecture.md` + `headroom/rtk/installer.py`:

- **RTK owns shell-command rewriting.** Headroom installs the prebuilt RTK binary
  and lets RTK's own Claude Code hook rewrite commands. It explicitly *rejected*
  invoking RTK in its own hot path.
- **Headroom surfaces RTK's savings** by polling `rtk gain --format json` and
  feeding deltas into its dashboard/metrics (`wrap_rtk_tokens_saved_per_session`).
- **Headroom compresses everything *downstream*** of RTK with its own engine.

ctxforge takes headroom's role:

1. **Install + own the RTK binary** (`ctxforge rtk install`) — download the pinned
   release, register RTK's hook. RTK now does Bash command rewriting (the thing it
   fires constantly on).
2. **Surface RTK savings on the ctxforge dashboard** — read `rtk gain --format
   json`, render an "RTK shell savings" plane, and persist deltas to `ops.log` so
   `ctxforge stats` and the dashboard reconcile. This is the user's explicit
   "see it working in the dashboard" requirement.
3. **Keep ctxforge's lane** — MCP sandbox tools (`ctx_execute`, `ctx_execute_file`,
   `ctx_search`, `graph_*`), session continuity, and SmartCrusher (`compress.rs`)
   as the downstream compaction. This mirrors headroom's "use lean-ctx as the
   context tool, compress downstream" split (ctxforge ≈ lean-ctx role).
4. **Coexistence rule: RTK owns Bash.** ctxforge's `PreToolUse` must pass Bash
   through whenever RTK is active, so the two hooks never double-rewrite a command.

## 2. Verified reference facts (do not re-derive; re-confirm in T0)

| Fact | Value | Source (read it) |
|---|---|---|
| RTK repo / license | `github.com/rtk-ai/rtk`, Apache-2.0 | `gh repo view rtk-ai/rtk` |
| Release assets | `releases/download/<ver>/rtk-<target>.<tar.gz\|zip>` | `headroom/rtk/installer.py` |
| Target triples | `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-musl` | `installer.py::_detect_runtime_target_triple` |
| Pinned version | `v0.28.2` (headroom's pin; T0 confirms it runs) | `headroom/rtk/__init__.py::RTK_VERSION` |
| Install dir (headroom-faithful) | `~/.ctxforge/bin/rtk` — a **global home** `<home>/bin`, exactly mirroring headroom's `bin_dir() = workspace_dir()/bin = ~/.headroom/bin`. Override via `$CTXFORGE_HOME` (≙ `$HEADROOM_WORKSPACE_DIR`); fallback to `which rtk`. **Distinct from** the per-project data dir `$CTXFORGE_DIR` (`<proj>/.ctxforge`). NB: `~/.rtk/bin` is RTK's *own* shim from `rtk init`, a separate artifact — not where the binary is shipped. | `headroom/paths.py::bin_dir` (`workspace_dir()/_BIN_DIR`), `headroom/rtk/__init__.py` |
| RTK self-installs its hook | `rtk init --target claude` (patches `settings.json` PreToolUse + writes RTK.md; `--hook-only`, `--uninstall` flags) | `rtk` `src/hooks/init.rs`, `src/main.rs::Init` |
| RTK records its own savings | SQLite history DB; cumulative | `rtk` `src/analytics/gain.rs` |
| `rtk gain --format json` shape | `{ "summary": { total_commands, total_input, total_output, total_saved, avg_savings_pct, total_time_ms, avg_time_ms }, "daily": [...]|null, "weekly": ..., "monthly": ... }` (pretty-printed; supports project scope) | `rtk` `src/analytics/gain.rs::export_json` + `ExportSummary` |

**Honest token/byte mapping (T2/T3).** RTK reports *tokens* (`total_saved`), not
bytes. The `rtk_shell` `OpRecord` must carry RTK's measured savings faithfully:
`tokens_saved_est = Δtotal_saved` (RTK's own number — **do not** re-apply
ctxforge's `/4` estimate). Put `total_input`/`total_output` in the record's
`input_summary`/note as token counts; the dedicated dashboard panel shows tokens,
not bytes, to avoid mislabeling.

## 3. Locked decisions

1. **Ship, don't port.** Download the prebuilt RTK binary; never vendor/re-author
   its compactors.
2. **RTK self-registers its hook** via `rtk init`; ctxforge shells out to it rather
   than re-implementing settings.json patching for RTK.
3. **RTK owns Bash; ctxforge defers.** When RTK is active, ctxforge `PreToolUse`
   returns passthrough for `tool=="Bash"`. ctxforge keeps WebFetch-deny / Read-nudge
   (non-Bash) under `CTXFORGE_ROUTING`.
4. **Network-free tests.** CI/integration tests use a **stub `rtk`** (a tiny script
   on `$PATH` that answers `--version` and `gain --format json` with canned JSON).
   The real download path (T1) is verified on-machine only, never in CI.
5. **Default off / additive.** RTK integration is opt-in (`ctxforge rtk install`);
   absent RTK, every new code path is a no-op and existing behavior is unchanged.
6. **Savings are RTK-measured.** The dashboard shows RTK's own `total_saved`, not a
   ctxforge re-estimate.

## 4. Task graph (parallel-safe; disjoint file ownership)

| ID | Title | Depends on | Writes (owned) | Read-only | Subagent | Completion predicate |
|----|-------|-----------|----------------|-----------|----------|----------------------|
| T0 | Recon + scaffold | none | `src/lib.rs` (+`pub mod rtk;`), `src/main.rs` (+`rtk` dispatch), `src/rtk/mod.rs` (submodule decls; `GainSummary` struct matching the verified JSON; `pub fn rtk_bin_path()->Option<PathBuf>`, `rtk_available()->bool`, `read_gain()`+`run_cli` **stubs**), stub `src/rtk/install.rs`, stub `src/rtk/gain.rs`, `RTK_NOTES.md` (captured real outputs) | `headroom/rtk/installer.py`, `rtk` `gain.rs`/`init.rs` | implement | `cargo build` green; `ctxforge rtk` prints usage; on this machine RTK v0.28.2 downloaded by hand, `rtk --version` + `rtk gain --format json` captured into `RTK_NOTES.md`; `GainSummary` deserializes that exact JSON (a unit test parses the captured sample) |
| T1 | `ctxforge rtk install/status/uninstall` | T0 | `src/rtk/install.rs` (target-triple detect; download `rtk-<triple>.<ext>` via `curl`; extract via `tar`/`unzip`; install to `~/.ctxforge/bin/rtk` (global home; override `$CTXFORGE_HOME/bin`); `chmod +x`; verify `rtk --version`; then `rtk init --target claude`; `status`/`uninstall`→`rtk init --uninstall`; honor existing `which rtk`) | `src/rtk/mod.rs`, headroom `installer.py` | implement | on-machine: `ctxforge rtk install` lands a working `rtk`, `ctxforge rtk status` reports installed+version+hook-registered; re-install is idempotent; `--version` override works |
| T2 | `rtk gain` bridge + `ctxforge rtk sync` | T0 | `src/rtk/gain.rs` (`read_gain(scope)`→`GainSummary` by running `rtk gain --format json`; `sync()` diffs vs watermark at `$CTXFORGE_DIR/rtk_watermark.json` and appends an `rtk_shell` `OpRecord` with `tokens_saved_est=Δtotal_saved`) | `src/obs` (`OpLog`/`OpRecord`), `src/rtk/mod.rs` | implement | with a **stub rtk** on PATH: `ctxforge rtk sync` writes one `rtk_shell` ops.log line whose `tokens_saved_est` == Δ`total_saved`; second sync with no new rtk activity writes no savings (watermark holds); `read_gain` parses the T0 sample |
| T3 | Dashboard + stats surfacing | T0 (uses `read_gain` stub) | `src/obs/stats.rs` (`snapshot_json` adds `"rtk"` block via `rtk::gain::read_gain()` best-effort; `mechanism("rtk_shell")=>"shell"`), `src/obs/dashboard.rs` (new "RTK shell savings" panel: commands, tokens saved, avg %, live-updating) | `src/rtk` | implement | `/api/stats` JSON contains an `rtk` object; route test renders the panel; `by_tool`/`by_mechanism` include `rtk_shell` when present; existing dashboard/stats tests still pass |
| T4 | Routing coexistence (RTK owns Bash) | T0 | `src/routing/mod.rs` (when `rtk::rtk_active()` — binary present **and** RTK hook in settings, or `CTXFORGE_DEFER_BASH_TO_RTK=1` — `route()` returns `Passthrough` for `tool=="Bash"`; WebFetch/Read/Grep unchanged) | `src/rtk` | implement | unit: RTK-active ⇒ `route("Bash", wrappable, full)==Passthrough` **and** `route("WebFetch",..,full)==Deny`; RTK-inactive ⇒ Bash wrap behaves exactly as today (existing routing tests green) |
| T5 | E2E + docs | T1,T2,T3,T4 | new `tests/rtk_tests.rs`, `README.md` (+`ctxforge rtk` cmds, the headroom-pattern division, the dashboard panel, the stub-rtk note) | all `src` | verify | full `cargo test` green; e2e (stub rtk): install-status → `rtk sync` → `ctxforge stats` & `/api/stats` show RTK savings → routing defers Bash when RTK active; MCP server stdout stays pure JSON-RPC; README updated |

### Notes on parallel safety
- T0 fixes the `src/rtk` **interface** (struct + fn signatures as stubs) so T1/T2/T3/T4
  compile against it independently; none re-touch `lib.rs`/`main.rs`/`rtk/mod.rs` after T0.
- File ownership is disjoint: T1=`install.rs`, T2=`gain.rs`, T3=`obs/{stats,dashboard}.rs`,
  T4=`routing/mod.rs`. Run wave 2 in separate `git worktree` checkouts.

## 5. Parallel waves

| Wave | Tasks | Concurrency | Notes |
|------|-------|-------------|-------|
| 1 | T0 | 1 | recon (capture real `rtk gain` JSON) + seams so the rest never touch `lib.rs`/`main.rs`/`rtk/mod.rs` |
| 2 | **T1, T2, T3, T4** | 4 | disjoint files (install / gain+sync / dashboard+stats / routing) — parallel sub-agents in worktrees |
| 3 | T5 | 1 | integration + docs; merges after T1–T4 land |

## 6. Verification (done-when — every clause machine-checkable)

- `cargo test` green; clean `cargo build` (no new warnings); `clippy` clean on new files.
- `ctxforge rtk install` (on-machine) downloads + installs the pinned RTK and runs
  `rtk init`; `ctxforge rtk status` shows installed + hook registered.
- With a stub `rtk`: `ctxforge rtk sync` writes an `rtk_shell` `ops.log` record whose
  `tokens_saved_est` equals the delta of `rtk gain`'s `total_saved`; idempotent on no-op.
- `ctxforge stats` and the dashboard `/api/stats` both show a non-zero **RTK shell
  savings** figure sourced from `rtk gain` (not a ctxforge re-estimate).
- The dashboard renders three planes: ctxforge MCP savings, RTK shell savings, session activity.
- With RTK active, a wrappable Bash `PreToolUse` returns `{}` (ctxforge defers); WebFetch
  still denies; with RTK inactive, prior routing behavior is unchanged.
- MCP server stdout stays pure JSON-RPC.
- `CTXFORGE_ROUTING=off` + no RTK installed ⇒ behavior identical to pre-plan (true no-op).

## 7. Kick off with /goal

```
/goal Read CTXFORGE_RTK_PLAN.md and execute the §4 task graph wave by wave: do T0 (recon + scaffold) yourself — actually download RTK v0.28.2 for this platform, capture `rtk --version` and `rtk gain --format json` into RTK_NOTES.md, and define GainSummary to match — then dispatch wave-2 tasks T1,T2,T3,T4 to parallel sub-agents in isolated git worktrees, then T5. Standing rule: before implementing each task, READ the cited RTK (rtk-ai/rtk) and headroom (chopratejas/headroom) source via gh — never guess. Build the headroom pattern: ctxforge ships/installs the RTK binary and surfaces its savings; RTK owns Bash command rewriting; ctxforge keeps its MCP/compaction/continuity lane and must never double-wrap Bash. Constraints: ship the prebuilt RTK binary (Apache-2.0), do not vendor/port its code; RTK self-registers its hook via `rtk init`; dashboard shows RTK's own measured `total_saved`, not a ctxforge re-estimate; tests are network-free via a stub `rtk`; everything additive and default-off (absent RTK = no-op); MCP server stdout stays pure JSON-RPC. Done when: cargo test green + clean build; `ctxforge rtk install/status/sync` work (install on-machine, sync against a stub rtk); a synced rtk_shell op carries tokens_saved_est == Δ rtk gain total_saved; `ctxforge stats` and the dashboard `/api/stats` show an RTK shell-savings plane; routing defers Bash to RTK when active and is unchanged when not; README documents the `ctxforge rtk` commands and the division of labor.
```

## 8. Risks / notes

- **RTK becomes a downloaded runtime dependency** (Apache-2.0, version-pinned) —
  same posture headroom ships with. `ctxforge rtk uninstall` reverses it.
- **Two PreToolUse hooks coexist** (RTK + ctxforge). T4's defer-Bash rule is the
  guard; verify RTK's hook and ctxforge's hook don't fight (RTK rewrites Bash,
  ctxforge passes Bash through and only touches WebFetch/Read).
- **`ctxforge session install` must not refuse RTK.** It currently blocks only
  Context Mode; confirm RTK coexistence is allowed (it is — different marker).
- **Network in T1** — gate behind explicit `ctxforge rtk install`; never auto-download.
- **The existing `CTXFORGE_ROUTING` Bash wrap (T2 of the routing plan) is superseded
  for Bash** by RTK but stays for the MCP `ctxforge wrap` offload path and non-Bash steering.
```
