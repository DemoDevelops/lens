pub fn queue_core(id: u64) -> u64 { id.wrapping_add(1) }
pub fn queue_drain_1(id: u64) -> u64 { queue_core(id) }
pub fn queue_drain_2(id: u64) -> u64 { queue_core(id) }
