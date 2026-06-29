pub fn metrics_sink(id: u64) -> u64 { id | 1 }
pub fn metrics_emit_1(id: u64) -> u64 { metrics_sink(id) }
pub fn metrics_emit_2(id: u64) -> u64 { metrics_sink(id) }
pub fn metrics_emit_3(id: u64) -> u64 { metrics_sink(id) }
pub fn metrics_emit_4(id: u64) -> u64 { metrics_sink(id) }
