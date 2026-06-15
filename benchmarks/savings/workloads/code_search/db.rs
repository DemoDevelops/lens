//! Database connection handling with retry.

use crate::config::Config;
use crate::logger::Logger;
use crate::retry::with_retry;

pub struct Db {
    url: String,
    connect_timeout: u64,
}

impl Db {
    pub fn connect(config: &Config, logger: &Logger) -> Result<Db, String> {
        logger.info("connect: opening database connection");
        let db = with_retry(config.retry_limit, || {
            if config.db_url.is_empty() {
                Err("connect error: empty db_url".to_string())
            } else {
                Ok(Db {
                    url: config.db_url.clone(),
                    connect_timeout: config.connect_timeout,
                })
            }
        });
        match &db {
            Ok(_) => logger.info("connect: database ready"),
            Err(e) => logger.error(&format!("connect failed: {}", e)),
        }
        db
    }

    pub fn query(&self, sql: &str) -> Result<Vec<String>, String> {
        if self.connect_timeout == 0 {
            return Err("query error: connection not ready".to_string());
        }
        let _ = &self.url;
        Ok(vec![format!("row for {}", sql)])
    }
}
