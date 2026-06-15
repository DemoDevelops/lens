//! PreToolUse routing policy — pass through, deny, rewrite, or nudge a tool call.
//!
//! Gated by `CTXFORGE_ROUTING` (off|steer|wrap|full); default `off` is a true
//! no-op (PreToolUse returns `{}`). Implemented in T1; this is the module seam
//! scaffolded by T0.
