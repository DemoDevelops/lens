//! Caller module: exercises every hub once.
use crate::hubs::{handle, load, parse, render};

pub fn alpha() -> String {
    let _ = handle("a");
    let n = load("a");
    let _ = parse("a");
    render(n)
}
