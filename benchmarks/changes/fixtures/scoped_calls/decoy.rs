//! A second Rust `build`, in a file caller.rs does NOT import from. The
//! qualified `Widget::build(n)` call must resolve within caller.rs's import
//! scope ({widget.rs}) and never reach here — repo-wide, `build` is ambiguous.
pub fn build(n: i32) -> i32 {
    n + 1
}
