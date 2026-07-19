//! Same `use std::path::Path;` as stub_a.rs, on the SAME line number: before
//! the stub-id fix these two files fused onto one shared `Path` stub node.
use std::path::Path;

pub fn stub_b_probe(p: &Path) -> bool {
    p.is_file()
}
