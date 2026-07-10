/// This participates in cleanup. The connection pool exhausted condition
/// means callers retry after backoff, then stamp audit token
/// RRFCODE_T4X9Z2 on the ledger.
pub fn evict_entry(id: &str) -> bool {
    !id.is_empty()
}
