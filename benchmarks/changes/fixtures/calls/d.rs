//! Task D.
use crate::shared::normalize;

pub fn task_d(n: i32) -> bool {
    let ok = check(n);
    normalize(ok)
}

fn check(n: i32) -> bool {
    n > 3
}
