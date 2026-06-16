# RTK_NOTES — T0 recon (verified on-machine, 2026-06-15)

Captured facts for the ctxforge ⇄ RTK integration (see `CTXFORGE_RTK_PLAN.md`).
Everything below was read from the **actual** RTK `v0.28.2` source / binary and
the headroom source — not guessed. Sub-agents: re-read the cited source before
implementing; this file is the frozen interface contract for wave 2.

## 1. Binary: download + install (T1)

- **Pinned version:** `v0.28.2` (headroom's pin — `headroom/rtk/__init__.py::RTK_VERSION`). Verified: it runs on this machine.
- **License:** Apache-2.0 (`rtk-ai/rtk`). Ship the prebuilt binary; do **not** vendor/port its Rust.
- **Release asset URL:** `https://github.com/rtk-ai/rtk/releases/download/<ver>/rtk-<triple>.<ext>`
- **Asset names (v0.28.2, verified via `gh release view`):**
  - `rtk-aarch64-apple-darwin.tar.gz`   ← **this machine** (Darwin arm64)
  - `rtk-x86_64-apple-darwin.tar.gz`
  - `rtk-aarch64-unknown-linux-gnu.tar.gz`
  - `rtk-x86_64-unknown-linux-musl.tar.gz`
  - `rtk-x86_64-pc-windows-msvc.zip`  (ext `zip`, binary `rtk.exe`)
- **Target-triple detection** (mirror `installer.py::_detect_runtime_target_triple`):
  - Darwin: `arm64`→`aarch64-apple-darwin`, else `x86_64-apple-darwin`
  - Linux: `aarch64`→`aarch64-unknown-linux-gnu`, else `x86_64-unknown-linux-musl`
  - Windows: `x86_64-pc-windows-msvc`
  - Override env: headroom uses `HEADROOM_RTK_TARGET`; ctxforge mirror → `CTXFORGE_RTK_TARGET`.
- **Archive layout (verified):** `tar tzf` shows a **single top-level `rtk`**. So
  `tar xzf <archive> -C <bindir>` lands `<bindir>/rtk` directly. `chmod +x` it.
- **Verify after extract:** run `<bindir>/rtk --version` → must print `rtk 0.28.2`.
- **Download size:** ~3.29 MB tarball; ~6.7 MB extracted binary.
- T1 uses `curl -fsSL <url> -o <tmp>` + `tar xzf`/`unzip` via `std::process::Command`
  (no new crate dependency). Verified working: see §3.

## 2. Install dir (headroom-faithful) — frozen in `src/rtk/mod.rs` (T0)

Mirrors headroom `paths.py`: `workspace_dir()` = `$HEADROOM_WORKSPACE_DIR` or
`~/.headroom`; `bin_dir()` = `workspace_dir()/bin`; `RTK_BIN_PATH` = `bin_dir()/rtk`.

ctxforge mapping (implemented in `rtk/mod.rs`):
- `home_root()` = `$CTXFORGE_HOME` if set, else `~/.ctxforge`  (≙ `$HEADROOM_WORKSPACE_DIR`)
- `bin_dir()`   = `home_root()/bin`  → binary at `~/.ctxforge/bin/rtk`
- `rtk_bin_path()` = `bin_dir()/rtk` if it exists, **else** `which rtk` (PATH scan)

> **Deviation from headroom (intentional):** headroom's `get_rtk_path()` checks
> `which rtk` **first**, managed dir second. ctxforge is **managed-first** so the
> pinned v0.28.2 binary is authoritative once installed (the "ship + own" posture),
> falling back to PATH only when not yet installed.

> `~/.ctxforge/bin/rtk` is the **global home** — distinct from the per-project data
> dir `$CTXFORGE_DIR` (`<proj>/.ctxforge`, holds ops.log/session.db). NB: `~/.rtk/bin`
> is RTK's *own* `rtk init` shim, a separate artifact — **not** where we ship the binary.

## 3. `rtk --version` (verified on this machine)

```
$ ~/.ctxforge/bin/rtk --version
rtk 0.28.2
```

## 4. `rtk gain --format json` — the shape `GainSummary` must match (T0/T2/T3)

Source: `rtk` `src/gain.rs` @ v0.28.2 — `ExportData` / `ExportSummary`
(`export_json` → `serde_json::to_string_pretty`). RTK reports **tokens**, not bytes.

```rust
// rtk src/gain.rs @ v0.28.2 (verbatim field set)
struct ExportData  { summary: ExportSummary,
                     daily: Option<Vec<DayStats>>,   // present only with --daily/--all
                     weekly: Option<Vec<WeekStats>>, // skip_serializing_if Option::is_none
                     monthly: Option<Vec<MonthStats>> }
struct ExportSummary { total_commands: usize, total_input: usize, total_output: usize,
                       total_saved: usize, avg_savings_pct: f64,
                       total_time_ms: u64, avg_time_ms: u64 }
```

**Captured verbatim (global scope) — this is the unit-test sample:**

```json
{
  "summary": {
    "total_commands": 3753,
    "total_input": 3689788,
    "total_output": 1424127,
    "total_saved": 2268362,
    "avg_savings_pct": 61.47675693020845,
    "total_time_ms": 2990161,
    "avg_time_ms": 796
  }
}
```

**Captured (project scope, `--project`, run from this repo):**

```json
{
  "summary": {
    "total_commands": 178,
    "total_input": 62638,
    "total_output": 21250,
    "total_saved": 41520,
    "avg_savings_pct": 66.28564130400076,
    "total_time_ms": 547523,
    "avg_time_ms": 3075
  }
}
```

Type notes that bit us if wrong:
- `avg_time_ms` is an **integer** (`u64`), not a float → struct field is `u64`.
- `total_saved` / `total_*` are non-negative integers → `u64`. Deltas are `i64`.
- `avg_savings_pct` is `f64`.
- With no period flag, `daily`/`weekly`/`monthly` keys are **absent** →
  `#[serde(default, skip_serializing_if = "Option::is_none")]`, typed `Option<Value>`.

`ctxforge`'s `GainSummary` (frozen in `src/rtk/mod.rs`) deserializes both samples;
`src/rtk/gain.rs::tests::gain_summary_deserializes_captured_sample` proves it.

**Honest mapping for the `rtk_shell` OpRecord (T2):** `tokens_saved_est = Δtotal_saved`
(RTK's own number — do **NOT** re-apply ctxforge's `/4` byte estimate). Stash
`total_input`/`total_output` (token counts) in `input_summary`/`note`. `raw_bytes_in`
/`bytes_returned` stay 0 (RTK measures tokens, not bytes) so the byte planes aren't polluted.

## 5. RTK CLI (verified — `rtk` `src/main.rs` @ v0.28.2)

> The plan §2 said `rtk init --target claude` — that flag **does not exist** in
> v0.28.2. Real flags below. Use headroom's invocation.

**`rtk init`** (`Commands::Init`):
`-g/--global` (patch `~/.claude/...`), `--show`, `--claude-md` (legacy), `--hook-only`
(hook, no RTK.md), `--auto-patch` (patch settings.json non-interactively), `--no-patch`,
`--uninstall` (remove all RTK artifacts — **requires `--global`**).
- **Register (mirror headroom `installer.py::register_claude_hooks`):**
  `rtk init --global --auto-patch`
- **Unregister:** `rtk init --global --uninstall`

**`rtk gain`** (`Commands::Gain`): `-p/--project` (scope to cwd), `--format <text|json|csv>`,
plus `--graph/--history/--quota/--daily/--weekly/--monthly/--all/--failures`.
- ctxforge calls: `rtk gain --format json` (global) / `rtk gain --format json --project`.

## 6. Hook registration + detection (`rtk` `src/init.rs` @ v0.28.2)

`rtk init` (unix) writes a hook **script** `~/.claude/hooks/rtk-rewrite.sh` and patches
`~/.claude/settings.json` with a PreToolUse entry (`prepare_hook_paths`, `patch_settings_json`):

```json
{ "hooks": { "PreToolUse": [{ "matcher": "Bash",
    "hooks": [{ "type": "command", "command": "<abs>/.claude/hooks/rtk-rewrite.sh" }] }] } }
```

Idempotency = `hook_already_present`; uninstall greps `command.contains("rtk-rewrite.sh")`.

**ctxforge detection (`rtk_hook_registered`, frozen in `rtk/mod.rs`):** read
`~/.claude/settings.json`, scan `hooks.PreToolUse[].hooks[].command` for any string
containing **`"rtk"`**. This catches v0.28.2's `rtk-rewrite.sh` **and** older `rtk hook`
markers. Settings path honors `$CLAUDE_CONFIG_DIR`, plus `$CTXFORGE_CLAUDE_SETTINGS`
(test seam).

**Live-machine state at T0:** `rtk` was NOT on PATH (binary absent), but
`~/.claude/settings.json` already had a stale `"command": "rtk hook claude"` PreToolUse
entry (leftover from a prior RTK install). `~/.ctxforge/` already existed (held `session.db`).
T0 installed the binary to `~/.ctxforge/bin/rtk`.

## 7. Coexistence rule — RTK owns Bash, ctxforge defers (T4)

`route()` must stay **pure** (the existing routing unit tests construct `RouteCtx`
directly and must keep passing **regardless of machine RTK state** — and the done-when
installs rtk on this very machine, which would otherwise flip global detection true).
Therefore:

- `RouteCtx` gains a precomputed `rtk_active: bool` field. `route()` returns
  `Decision::Passthrough` for `tool == "Bash"` when `ctx.rtk_active` (WebFetch/Read/Grep
  unchanged — WebFetch still denies under steer). The `rc()` test helper sets it `false`;
  one new unit test sets it `true`.
- The live wiring is one line in `src/session/hook.rs` PreToolUse:
  `rtk_active: rtk::rtk_active(&data_dir)`. (hook.rs is touched by **no other wave-2 task**,
  so this stays parallel-safe.)
- `rtk::rtk_active(_data_dir)` = **env override wins** (deterministic, mirrors
  `CTXFORGE_ROUTING_MCP`): `CTXFORGE_DEFER_BASH_TO_RTK` truthy ⇒ true, falsey ⇒ false;
  unset ⇒ `rtk_available() && rtk_hook_registered()`.
- **T5 must** add `.env("CTXFORGE_DEFER_BASH_TO_RTK", "0")` to the existing
  `tests/routing_tests.rs::run_hook` harness so those Bash-wrapping integration tests stay
  green after rtk is installed on the machine.

## 8. Tests are network-free via a **stub `rtk`** (T2/T3/T5)

No test downloads. Point resolution at a stub by setting `CTXFORGE_HOME=<tempdir>` and
writing an executable `<tempdir>/bin/rtk` that answers:
- `--version` → `rtk 0.28.2`
- `gain --format json` → canned `GainSummary` JSON (e.g. the §4 sample, or a small one)

Then `rtk_bin_path()` resolves the stub (managed-first), `read_gain()` parses its output,
and `rtk_active` is forced via `CTXFORGE_DEFER_BASH_TO_RTK`. The real download path (T1) is
verified **on-machine only** (already done in T0).

## 9. Interface frozen by T0 (everyone compiles against this; do not re-touch `mod.rs`)

`src/rtk/mod.rs` (T0 — stable after T0):
- `pub const RTK_VERSION: &str = "v0.28.2";`  `pub const RTK_EXE` (`rtk`/`rtk.exe`)
- `pub struct GainSummary { summary: ExportSummary, daily/weekly/monthly: Option<Value> }`
- `pub struct ExportSummary { total_commands,total_input,total_output,total_saved: u64,
  avg_savings_pct: f64, total_time_ms,avg_time_ms: u64 }`
- `pub fn home_root() -> Option<PathBuf>` / `pub fn bin_dir() -> Option<PathBuf>`
- `pub fn rtk_bin_path() -> Option<PathBuf>` (managed-first, then PATH)
- `pub fn rtk_available() -> bool`
- `pub fn claude_settings_path() -> Option<PathBuf>` / `pub fn rtk_hook_registered() -> bool`
- `pub fn rtk_active(_data_dir: &Path) -> bool`  (env override, else available && hook)
- `pub fn run_rtk(args: &[&str]) -> Result<std::process::Output>`  (shared CLI runner)
- `pub fn run_cli(args: &[String]) -> Result<()>`  (`ctxforge rtk install|status|uninstall|sync`)

`src/rtk/install.rs` — **T1 owns**: `pub fn install()/status()/uninstall() -> Result<()>`
(T0 stubs that `bail!`).

`src/rtk/gain.rs` — **T2 owns**: `pub enum Scope { Global, Project }`,
`pub fn parse_gain(&str) -> Result<GainSummary>` (T0, real + tested),
`pub fn read_gain(Scope) -> Result<GainSummary>` (T0 implements: runs `rtk gain` + parses —
so T3 can test against a stub), `pub fn sync() -> Result<()>` (T0 stubs `bail!`; **T2 implements**:
watermark at `$CTXFORGE_DIR/rtk_watermark.json` + `rtk_shell` OpRecord with
`tokens_saved_est = Δtotal_saved`).

`src/lib.rs` (+`pub mod rtk;`), `src/main.rs` (+`Some("rtk") => rtk::run_cli`).

## 10. Sources read at T0 (re-read before implementing)

- `gh repo view rtk-ai/rtk`; `gh release view v0.28.2 -R rtk-ai/rtk`
- `rtk` @ v0.28.2: `src/gain.rs` (JSON shape), `src/init.rs` (hook), `src/main.rs` (CLI)
- `chopratejas/headroom`: `headroom/rtk/installer.py`, `headroom/rtk/__init__.py`, `headroom/paths.py`
