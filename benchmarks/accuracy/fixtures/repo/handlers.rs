//! Request handling.

use crate::auth::authenticate;
use crate::db::fetch_user;

pub fn handle_request(token: &str, key: &str) -> Result<String, String> {
    authenticate(token)?;
    let user = fetch_user(key)?;
    Ok(user)
}
