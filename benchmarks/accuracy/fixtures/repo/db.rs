//! Database access.

pub struct Connection {
    pub url: String,
}

pub fn connect_db() -> Connection {
    Connection {
        url: "postgres://db:5432/app".to_string(),
    }
}

pub fn fetch_user(key: &str) -> Result<String, String> {
    let conn = connect_db();
    if conn.url.is_empty() {
        return Err("no connection".to_string());
    }
    Ok(format!("user for {}", key))
}
