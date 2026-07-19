//! Same-file dual `new` (the live `Graph::new` / `Node::new` shape): extraction
//! surfaces both calls as the bare `new`, which matches BOTH same-file
//! definitions — ambiguous, so scoped resolution must emit NO `new` edge at all
//! (the old resolver fanned out to both).
pub struct Graphish;

impl Graphish {
    pub fn new() -> Graphish {
        Graphish
    }
}

pub struct Nodeish;

impl Nodeish {
    pub fn new() -> Nodeish {
        Nodeish
    }
}

pub fn make() -> (Graphish, Nodeish) {
    (Graphish::new(), Nodeish::new())
}
