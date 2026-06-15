//! Structured logging used across the service.

use crate::config::Config;

pub struct Logger {
    level: String,
    target: String,
}

impl Logger {
    pub fn from_config(config: &Config) -> Logger {
        Logger {
            level: config.log_level.clone(),
            target: "stderr".to_string(),
        }
    }

    pub fn error(&self, msg: &str) {
        eprintln!("[{}] ERROR {}: {}", self.target, self.level, msg);
    }

    pub fn warn(&self, msg: &str) {
        eprintln!("[{}] WARN: {}", self.target, msg);
    }

    pub fn info(&self, msg: &str) {
        eprintln!("[{}] INFO: {}", self.target, msg);
    }

    pub fn request(&self, method: &str, path: &str) {
        self.info(&format!("request {} {}", method, path));
    }
}
