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
//!
//! Env flags — kill-switch polarity: every flag is ON by default and `=0`
//! disables it (the `LENS_GREP_FIRST_DENY` pattern), one flag per arm:
//! - `LENS_GREP_SYMBOL_NUDGE` / `LENS_GREP_SYMBOL_DENY`
//! - `LENS_READ_SKELETON_NUDGE` / `LENS_READ_SKELETON_DENY`
//! - `LENS_BASH_AGG_NUDGE` / `LENS_BASH_AGG_DENY`
//! - `LENS_EDIT_LINKS_NUDGE` / `LENS_EDIT_LINKS_DENY`
//! - `LENS_GREP_AST_NUDGE` / `LENS_GREP_AST_DENY`
//! - `LENS_READ_OVERVIEW_NUDGE` / `LENS_READ_OVERVIEW_DENY`
//!
//! Every rail carries BOTH a nudge and a deny arm. The deny arm fires under
//! `Level::steers` (Steer|Full). The nudge arm fires only at `Level::Nudge`
//! (`nudges() && !steers()`) for gsym/rskel/rovr/elink; the gast/bagg nudges
//! keep their original `Level::nudges` gate, pre-empted at steering levels by
//! their deny's shared one-shot key. Both arms of a rail share that rail's
//! one-shot throttle key AND the SAME `{p}_would_fire` / `{p}_next_{class}`
//! counter keys — the prefix identifies the rail; `{p}_shadow_next_{class}` is
//! the follower counter when the operator has kill-switched the arm (`=0`).

pub mod bash_aggregate;
pub mod edit_callers;
pub mod grep_ast;
pub mod grep_symbol;
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
