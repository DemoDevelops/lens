//! The hubs. Each of these is called from every caller module below, so each has
//! a high in-degree. A name-only query that id-sorts its matches buries them among
//! the same-substring decoys; importance ranking surfaces them first.
pub fn handle(req: &str) -> bool {
    !req.is_empty()
}

pub fn load(key: &str) -> usize {
    key.len()
}

pub fn render(data: usize) -> String {
    format!("{data}")
}

pub fn parse(s: &str) -> usize {
    s.len()
}
