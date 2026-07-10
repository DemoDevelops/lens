use crate::hub::evict_entry;

pub fn task_a(id: &str) -> bool {
    evict_entry(id)
}
