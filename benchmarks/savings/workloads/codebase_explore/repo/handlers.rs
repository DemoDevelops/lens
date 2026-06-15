//! Request handling: ties auth and db together.

use crate::auth::validate_token;
use crate::db::{fetch_user, Connection};
use crate::util::format_row;

pub fn handle_request(conn: &Connection, token: &str, key: &str) -> Result<String, String> {
    validate_token(token)?;
    let user = fetch_user(conn, key)?;
    Ok(format_row(&user))
}

pub fn handle_batch(conn: &Connection, token: &str, keys: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for key in keys {
        if let Ok(row) = handle_request(conn, token, key) {
            out.push(row);
        }
    }
    out
}
