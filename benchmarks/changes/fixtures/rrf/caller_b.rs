use crate::hub::evict_entry;

pub fn task_b(id: &str) -> bool {
    evict_entry(id)
}
