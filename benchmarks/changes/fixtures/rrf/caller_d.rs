use crate::hub::evict_entry;

pub fn task_d(id: &str) -> bool {
    evict_entry(id)
}
