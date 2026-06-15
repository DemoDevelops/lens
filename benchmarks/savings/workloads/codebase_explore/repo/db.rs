//! Database access.

pub struct Connection {
    pub url: String,
}

pub fn connect_db(url: &str) -> Connection {
    Connection {
        url: url.to_string(),
    }
}

pub fn fetch_user(conn: &Connection, key: &str) -> Result<String, String> {
    if conn.url.is_empty() {
        return Err("no connection".to_string());
    }
    Ok(format!("{{user for {}}}", key))
}
