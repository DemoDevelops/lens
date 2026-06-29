pub fn cache_ring(id: u64) -> u64 { id.rotate_left(7) }
pub fn cache_scan_1(id: u64) -> u64 { cache_ring(id) }
pub fn cache_scan_2(id: u64) -> u64 { cache_ring(id) }
pub fn cache_scan_3(id: u64) -> u64 { cache_ring(id) }
