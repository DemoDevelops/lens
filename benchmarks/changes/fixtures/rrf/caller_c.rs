use crate::hub::evict_entry;

pub fn task_c(id: &str) -> bool {
    evict_entry(id)
}
