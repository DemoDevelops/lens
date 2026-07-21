//! lens tool-routing rail family — one stateless classifier per rail.
//!
//! ## Counter-key + env-flag contract
//!
//! Per rail, `bump_stat` keys are `{p}_would_fire` and `{p}_next_{class}` where
//! `class ∈ {lens|grep|read|bash|edit|other}`.
//! `{p}_shadow_next_{class}` is the shadow-arm follower (rail flag OFF); `{p}_next_{class}` is the live-arm one.
//!
//! Prefixes:
//! - `grep_symbol` → `gsym`
//! - `read_skeleton` → `rskel`
//! - `bash_aggregate` → `bagg`
//! - `edit_callers` → `elink`
//! - `grep_ast` → `gast`
//! - `read_overview` → `rovr`
//! - `graph_reverify` → `grevf`
//! - `atomic_chain` → `achain`
//! - `overview_rebuy` → `ovrb`
//!
//! Env flags — kill-switch polarity: every flag is ON by default and `=0`
//! disables it (the `LENS_GREP_FIRST_DENY` pattern), one flag per rail:
//! - `LENS_GREP_SYMBOL_DENY`
//! - `LENS_READ_SKELETON_DENY`
//! - `LENS_BASH_AGG_DENY`
//! - `LENS_EDIT_LINKS_DENY`
//! - `LENS_GREP_AST_DENY`
//! - `LENS_READ_OVERVIEW_DENY`
//! - `LENS_BASH_GREP_DENY`
//! - `LENS_READ_RUNFILE_DENY`
//! - `LENS_GRAPH_REVERIFY`
//! - `LENS_ATOMIC_CHAIN_DENY`
//! - `LENS_OVERVIEW_REBUY_DENY`
//!
//! Every rail is DENY-only, firing under `Level::steers` (Steer|Full). The
//! nudge arms were retired 2026-07-19 (measured conversion 0-33% for nudges
//! vs 51-71% for denies). Each rail keeps its one-shot throttle key and the
//! `{p}_would_fire` / `{p}_next_{class}` counter keys — the prefix identifies
//! the rail; `{p}_shadow_next_{class}` is the follower counter when the
//! operator has kill-switched the rail (`=0`). `grevf` is per-FILE rather than
//! per-symbol/session (see `graph_reverify`'s doc); its `would_fire`/`next`
//! counters are enumerated in `obs::stats::REROUTE_PREFIXES` but, unlike its
//! siblings, are not yet emitted from `session::hook`'s shadow-counter plane —
//! a deferred follow-up. `achain` denies per drift EPISODE (its consecutive
//! counter resets on fire, `inspect_escalation`-style) rather than once per
//! session, and shares `grevf`'s deferred-counter status. `ovrb` denies an
//! unfocused `lens_overview` re-buy once per session when the SessionStart
//! digest was injected (`ovrb:digest`), standing down when `rovr` already
//! pushed toward overview.

pub mod atomic_chain;
pub mod bash_aggregate;
pub mod bash_grep;
pub mod edit_callers;
pub mod graph_reverify;
pub mod grep_ast;
pub mod grep_symbol;
pub mod overview_rebuy;
pub mod read_overview;
pub mod read_skeleton;

/// Canonical prefix strings consumed by T8 (integration) and T9 (obs).
#[allow(dead_code)]
pub const PREFIX_GREP_SYMBOL: &str = "gsym";
#[allow(dead_code)]
pub const PREFIX_READ_SKELETON: &str = "rskel";
#[allow(dead_code)]
pub const PREFIX_BASH_AGG: &str = "bagg";
#[allow(dead_code)]
pub const PREFIX_EDIT_LINKS: &str = "elink";
#[allow(dead_code)]
pub const PREFIX_GREP_AST: &str = "gast";
#[allow(dead_code)]
pub const PREFIX_READ_OVERVIEW: &str = "rovr";
#[allow(dead_code)]
pub const PREFIX_GRAPH_REVERIFY: &str = "grevf";
#[allow(dead_code)]
pub const PREFIX_ATOMIC_CHAIN: &str = "achain";
#[allow(dead_code)]
pub const PREFIX_OVERVIEW_REBUY: &str = "ovrb";
