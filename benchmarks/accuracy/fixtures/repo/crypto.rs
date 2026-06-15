//! Key management.

pub struct KeyRing {
    pub keys: Vec<String>,
}

pub fn rotate_keys(old: &str) -> String {
    format!("rotated:{}", old)
}

pub fn fingerprint(key: &str) -> usize {
    key.len()
}
