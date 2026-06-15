//! Small utilities.

pub struct Buffer {
    pub data: Vec<u8>,
}

pub fn format_row(row: &str) -> String {
    format!("row({})", row)
}

pub fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
