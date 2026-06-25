//! Task B.
use crate::shared::normalize;

pub fn task_b(n: i32) -> bool {
    let ok = check(n);
    normalize(ok)
}

fn check(n: i32) -> bool {
    n > 1
}
