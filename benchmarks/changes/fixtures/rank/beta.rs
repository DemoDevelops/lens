//! Caller module: exercises every hub once.
use crate::hubs::{handle, load, parse, render};

pub fn beta() -> String {
    let _ = handle("b");
    let n = load("b");
    let _ = parse("b");
    render(n)
}
