//! Same adversarial structure as nullB, but the find budget is K=1 — the tight
//! regime where personalized PR's PR-primary order can push the high-lexical
//! answer out of a single-slot budget entirely.
pub fn audit_validate(t: u64) -> bool { t > 0 }
pub fn audit_log(m: u64) -> u64 { m }
pub fn audit_sink_1(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_2(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_3(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_4(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_5(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_6(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_7(m: u64) -> u64 { audit_log(m) }
pub fn audit_sink_8(m: u64) -> u64 { audit_log(m) }
