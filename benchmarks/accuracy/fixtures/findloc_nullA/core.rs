//! Easy case: the answer `rotate_keys` is both a strong lexical match for
//! "rotate keys" AND central (called by 4 workers). Raw already has it in budget.
pub fn rotate_keys(seed: u64) -> u64 { seed ^ 0xdeadbeef }
pub fn rotate_worker_1(s: u64) -> u64 { rotate_keys(s) }
pub fn rotate_worker_2(s: u64) -> u64 { rotate_keys(s) }
pub fn rotate_worker_3(s: u64) -> u64 { rotate_keys(s) }
pub fn rotate_worker_4(s: u64) -> u64 { rotate_keys(s) }
