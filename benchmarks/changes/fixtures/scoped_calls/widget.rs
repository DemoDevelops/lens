//! Target of caller.rs's qualified `Widget::build(n)` call.
pub struct Widget {
    pub n: i32,
}

impl Widget {
    pub fn build(n: i32) -> i32 {
        n
    }
}
