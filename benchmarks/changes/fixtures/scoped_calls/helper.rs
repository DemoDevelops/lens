//! Rust `shared_name`: the SAME name exists in helper.py. A Rust caller must
//! resolve here, never cross-language.
pub fn shared_name(n: i32) -> i32 {
    n
}
