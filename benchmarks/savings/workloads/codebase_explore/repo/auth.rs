//! Authentication helpers.

pub struct Session {
    pub user: String,
}

pub fn validate_token(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("empty token".to_string());
    }
    Ok(())
}

pub fn rotate_keys(old: &str) -> String {
    format!("rotated:{}", old)
}
