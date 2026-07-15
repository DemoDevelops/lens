//! C40 fixture (L40 aider-style overview focus): two hubs called by 40 workers
//! dominate global importance. `lonely.rs` holds one isolated helper with no
//! callers or callees, globally unimportant and excluded from a budget sized
//! exactly to the two hubs, unless its file is marked touched. See `lonely.rs`.
pub fn hub_x() -> i32 { 1 }
pub fn hub_y() -> i32 { 2 }

pub fn worker_00() -> i32 { hub_x() + hub_y() }
pub fn worker_01() -> i32 { hub_x() + hub_y() }
pub fn worker_02() -> i32 { hub_x() + hub_y() }
pub fn worker_03() -> i32 { hub_x() + hub_y() }
pub fn worker_04() -> i32 { hub_x() + hub_y() }
pub fn worker_05() -> i32 { hub_x() + hub_y() }
pub fn worker_06() -> i32 { hub_x() + hub_y() }
pub fn worker_07() -> i32 { hub_x() + hub_y() }
pub fn worker_08() -> i32 { hub_x() + hub_y() }
pub fn worker_09() -> i32 { hub_x() + hub_y() }
pub fn worker_10() -> i32 { hub_x() + hub_y() }
pub fn worker_11() -> i32 { hub_x() + hub_y() }
pub fn worker_12() -> i32 { hub_x() + hub_y() }
pub fn worker_13() -> i32 { hub_x() + hub_y() }
pub fn worker_14() -> i32 { hub_x() + hub_y() }
pub fn worker_15() -> i32 { hub_x() + hub_y() }
pub fn worker_16() -> i32 { hub_x() + hub_y() }
pub fn worker_17() -> i32 { hub_x() + hub_y() }
pub fn worker_18() -> i32 { hub_x() + hub_y() }
pub fn worker_19() -> i32 { hub_x() + hub_y() }
pub fn worker_20() -> i32 { hub_x() + hub_y() }
pub fn worker_21() -> i32 { hub_x() + hub_y() }
pub fn worker_22() -> i32 { hub_x() + hub_y() }
pub fn worker_23() -> i32 { hub_x() + hub_y() }
pub fn worker_24() -> i32 { hub_x() + hub_y() }
pub fn worker_25() -> i32 { hub_x() + hub_y() }
pub fn worker_26() -> i32 { hub_x() + hub_y() }
pub fn worker_27() -> i32 { hub_x() + hub_y() }
pub fn worker_28() -> i32 { hub_x() + hub_y() }
pub fn worker_29() -> i32 { hub_x() + hub_y() }
pub fn worker_30() -> i32 { hub_x() + hub_y() }
pub fn worker_31() -> i32 { hub_x() + hub_y() }
pub fn worker_32() -> i32 { hub_x() + hub_y() }
pub fn worker_33() -> i32 { hub_x() + hub_y() }
pub fn worker_34() -> i32 { hub_x() + hub_y() }
pub fn worker_35() -> i32 { hub_x() + hub_y() }
pub fn worker_36() -> i32 { hub_x() + hub_y() }
pub fn worker_37() -> i32 { hub_x() + hub_y() }
pub fn worker_38() -> i32 { hub_x() + hub_y() }
pub fn worker_39() -> i32 { hub_x() + hub_y() }
