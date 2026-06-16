//! `ctxforge rtk install | status | uninstall` — manage the prebuilt RTK binary
//! and its Claude Code hook (headroom pattern; see `RTK_NOTES.md` §1/§5/§6).
//!
//! **T0 scaffold:** these are stubs that compile so the `rtk::run_cli` dispatcher
//! and the rest of the crate build. **T1 implements** them: target-triple detect,
//! `curl` download of `rtk-<triple>.<ext>`, `tar`/`unzip` extract to
//! `~/.ctxforge/bin/rtk`, `chmod +x`, verify `rtk --version`, then register the
//! hook via `rtk init --global --auto-patch`. `uninstall` → `rtk init --global
//! --uninstall`; an existing `which rtk` is honored. The network download path is
//! verified on-machine only (T0 already proved it), never in CI.

use anyhow::{bail, Result};

/// Download + install the pinned RTK binary and register its Claude hook.
pub fn install() -> Result<()> {
    bail!("ctxforge rtk install: not yet implemented (T1)")
}

/// Report install state: binary path, version, and hook registration.
pub fn status() -> Result<()> {
    bail!("ctxforge rtk status: not yet implemented (T1)")
}

/// Remove RTK's Claude hook (`rtk init --global --uninstall`).
pub fn uninstall() -> Result<()> {
    bail!("ctxforge rtk uninstall: not yet implemented (T1)")
}
