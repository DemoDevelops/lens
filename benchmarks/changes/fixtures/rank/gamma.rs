//! Caller module: exercises every hub once.
use crate::hubs::{handle, load, parse, render};

pub fn gamma() -> String {
    let _ = handle("c");
    let n = load("c");
    let _ = parse("c");
    render(n)
}
