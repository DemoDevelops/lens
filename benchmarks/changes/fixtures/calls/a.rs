//! Task A. `check` is defined here AND in b/c/d/e with the same name, so a
//! name-only call resolver links every `check()` call to all five — the spurious
//! cross-file edges scope-aware resolution must drop.
use crate::shared::normalize;

pub fn task_a(n: i32) -> bool {
    let ok = check(n);
    normalize(ok)
}

fn check(n: i32) -> bool {
    n > 0
}
