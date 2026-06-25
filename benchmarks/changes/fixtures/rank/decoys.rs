//! Decoys. Each shares a name substring with a hub but is never called, so each
//! has a low degree. These are what a name-only ranker can rank above the hubs.
pub fn handle_legacy_unused() -> bool {
    false
}

pub fn handle_noop() -> bool {
    true
}

pub fn load_unused_helper() -> usize {
    0
}

pub fn load_temp() -> usize {
    1
}

pub fn render_stub() -> String {
    String::new()
}

pub fn render_old() -> String {
    String::new()
}

pub fn parse_unused() -> usize {
    0
}

pub fn parse_dead() -> usize {
    0
}
