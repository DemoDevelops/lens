//! Same `use std::path::Path;` as stub_b.rs, on the SAME line number: the
//! unresolved-import stub minted here must be a distinct node from stub_b's.
use std::path::Path;

pub fn stub_a_probe(p: &Path) -> bool {
    p.exists()
}
