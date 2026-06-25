//! Multi-symbol import (C9) plus trait signatures, an associated const, and a
//! type alias (C10). A last-token import resolver links only `Gamma`; per-symbol
//! resolution must emit one import edge each for Alpha, Beta, and Gamma. The
//! trait method signatures, the const, and the type alias are def kinds the base
//! Rust query does not capture.
use crate::shared::{Alpha, Beta, Gamma};

pub trait Service {
    fn start(&self) -> bool;
    fn stop(&self) -> bool;
    type Output;
    const MAX: usize;
}

pub const LIMIT: usize = 10;
pub type Alias = usize;

pub fn use_them() -> bool {
    let _ = (Alpha, Beta, Gamma);
    true
}
