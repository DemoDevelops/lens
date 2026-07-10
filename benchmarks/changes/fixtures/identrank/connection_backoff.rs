pub fn schedule_retry(attempt: u32) -> u32 {
    retryConnectionBackoff(attempt)
}
