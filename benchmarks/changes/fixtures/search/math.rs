//! Arithmetic. Carries the operator/path/keyword tokens the trigram path must be
//! able to match literally: `std::fs`, the `->` return arrow, and `fn add`.
use std::fs;

pub fn add(a: i32, b: i32) -> i32 {
    let _ = std::fs::metadata(".");
    a + b
}
