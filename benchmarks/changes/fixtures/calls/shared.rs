//! Shared helper imported by every task module. `normalize` is defined exactly
//! once, so the only correct resolution of an imported `normalize()` call is to
//! this definition (scope-aware import resolution, not same-file).
pub fn normalize(b: bool) -> bool {
    b
}
