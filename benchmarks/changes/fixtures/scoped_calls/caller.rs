//! C43 driver. `Widget::build(n)` is a qualified cross-file call: extraction
//! surfaces only the bare `build`, so resolution must recover the target via
//! the import of `Widget` (the L15 qualifier proxy) — exactly one edge, to
//! widget.rs. `shared_name` is defined in BOTH helper.rs (Rust) and helper.py
//! (Python); language scoping must link the Rust definition only.
use crate::widget::Widget;

pub fn drive(n: i32) -> i32 {
    let w = Widget::build(n);
    shared_name(w)
}
