//! Authentication.

pub fn authenticate(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("empty token".to_string());
    }
    Ok(())
}
