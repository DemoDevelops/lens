/// This function participates in session cleanup. When the connection layer
/// reports a pool exhausted condition, callers should retry only after an
/// exponential backoff.
pub fn evict_entry(id: &str) -> bool {
    !id.is_empty()
}
