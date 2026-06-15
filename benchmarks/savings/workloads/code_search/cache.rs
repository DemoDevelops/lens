//! In-memory cache with a configurable TTL.

use std::collections::HashMap;

use crate::config::Config;

pub struct Cache {
    entries: HashMap<String, String>,
    ttl: u64,
}

impl Cache {
    pub fn new(config: &Config) -> Cache {
        Cache {
            entries: HashMap::new(),
            ttl: config.cache_ttl,
        }
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.entries.get(key)
    }

    pub fn set(&mut self, key: &str, value: &str) {
        if self.ttl == 0 {
            return; // cache disabled in config
        }
        self.entries.insert(key.to_string(), value.to_string());
    }

    pub fn invalidate(&mut self, key: &str) {
        self.entries.remove(key);
    }
}
