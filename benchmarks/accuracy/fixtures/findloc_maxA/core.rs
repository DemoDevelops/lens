//! Hub `order_gateway` (low-lexical: matches only "order") called by 8 intake
//! handlers. Strong "order processor" decoys live in decoys.rs.
pub fn order_gateway(id: u64) -> u64 { id.wrapping_mul(2654435761) }
pub fn order_intake_1(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_2(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_3(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_4(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_5(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_6(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_7(id: u64) -> u64 { order_gateway(id) }
pub fn order_intake_8(id: u64) -> u64 { order_gateway(id) }
