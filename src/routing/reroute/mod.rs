//! lens tool-routing rail family — one stateless classifier per rail.
//!
//! ## Counter-key + env-flag contract
//!
//! Per rail, `bump_stat` keys are `{p}_would_fire` and `{p}_next_{class}` where
//! `class ∈ {lens|grep|read|bash|edit|other}`.
//!
//! Prefixes:
//! - `grep_symbol` → `gsym`
//! - `read_skeleton` → `rskel`
//! - `bash_aggregate` → `bagg`
//! - `edit_callers` → `elink`
//! - `grep_ast` → `gast`
//! - `read_overview` → `rovr`
//!
//! Env flags (all default OFF):
//! - `LENS_GREP_SYMBOL_DENY`
//! - `LENS_READ_SKELETON_DENY`
//! - `LENS_BASH_AGG_NUDGE`
//! - `LENS_EDIT_LINKS_NUDGE`
//! - `LENS_GREP_AST_NUDGE`
//! - `LENS_READ_OVERVIEW_NUDGE`

pub mod grep_symbol;
pub mod read_skeleton;
pub mod bash_aggregate;
pub mod edit_callers;
pub mod grep_ast;
pub mod read_overview;

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
