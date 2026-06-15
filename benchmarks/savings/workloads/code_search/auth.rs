//! Authentication and request validation.

use crate::config::Config;
use crate::logger::Logger;

pub struct Auth {
    secret: String,
}

impl Auth {
    pub fn new(config: &Config) -> Auth {
        Auth {
            secret: config.auth_secret.clone(),
        }
    }

    pub fn validate(&self, token: &str, logger: &Logger) -> Result<(), String> {
        if token.is_empty() {
            logger.error("auth error: empty token on request");
            return Err("auth: empty token".to_string());
        }
        if !token.starts_with(&self.secret) {
            logger.warn("auth: token did not match secret");
            return Err("auth: invalid token".to_string());
        }
        Ok(())
    }

    pub fn rotate_secret(&mut self, new_secret: &str) {
        self.secret = new_secret.to_string();
    }
}
