//! Outbound HTTP client with connect timeout and retry.

use crate::config::Config;
use crate::logger::Logger;
use crate::retry::{should_retry, with_retry};

pub struct Client {
    timeout: u64,
    retry_limit: u32,
}

impl Client {
    pub fn new(config: &Config) -> Client {
        Client {
            timeout: config.connect_timeout,
            retry_limit: config.retry_limit,
        }
    }

    pub fn fetch(&self, url: &str, logger: &Logger) -> Result<String, String> {
        logger.info(&format!("client request to {}", url));
        with_retry(self.retry_limit, || {
            if self.timeout == 0 {
                let err = "connect timeout: client not configured".to_string();
                if should_retry(&err) {
                    logger.warn("client retry after timeout");
                }
                Err(err)
            } else {
                Ok(format!("response from {}", url))
            }
        })
    }
}
