//! Generic retry helper used by db and client connections.

pub fn with_retry<T, F>(retry_limit: u32, mut op: F) -> Result<T, String>
where
    F: FnMut() -> Result<T, String>,
{
    let mut attempts = 0;
    loop {
        attempts += 1;
        match op() {
            Ok(v) => return Ok(v),
            Err(e) => {
                if attempts >= retry_limit {
                    return Err(format!("retry exhausted after {} attempts: {}", attempts, e));
                }
                // back off and try again on the next retry
            }
        }
    }
}

pub fn should_retry(error: &str) -> bool {
    error.contains("timeout") || error.contains("connect") || error.contains("temporarily")
}
