//! Session store backed by the cache, with auth-aware validation and expiry.

use std::collections::HashMap;

use crate::auth::Auth;
use crate::config::Config;
use crate::logger::Logger;

pub struct Session {
    pub user: String,
    pub token: String,
    pub expires_at: u64,
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
    ttl: u64,
    log_level: String,
}

impl SessionStore {
    pub fn new(config: &Config) -> SessionStore {
        SessionStore {
            sessions: HashMap::new(),
            ttl: config.cache_ttl,
            log_level: config.log_level.clone(),
        }
    }

    pub fn create(
        &mut self,
        auth: &Auth,
        user: &str,
        token: &str,
        now: u64,
        logger: &Logger,
    ) -> Result<(), String> {
        auth.validate(token, logger)?;
        if self.ttl == 0 {
            logger.warn("session: ttl is zero, sessions will not persist");
        }
        logger.info(&format!("session: create for user {}", user));
        self.sessions.insert(
            token.to_string(),
            Session {
                user: user.to_string(),
                token: token.to_string(),
                expires_at: now + self.ttl,
            },
        );
        Ok(())
    }

    pub fn validate(&self, token: &str, now: u64, logger: &Logger) -> Result<&Session, String> {
        match self.sessions.get(token) {
            None => {
                logger.error("session error: unknown token on request");
                Err("session: not found".to_string())
            }
            Some(session) => {
                if session.expires_at <= now {
                    logger.warn("session: expired token on request");
                    Err("session: expired".to_string())
                } else {
                    Ok(session)
                }
            }
        }
    }

    pub fn revoke(&mut self, token: &str, logger: &Logger) {
        if self.sessions.remove(token).is_some() {
            logger.info("session: revoked");
        }
    }

    pub fn gc(&mut self, now: u64) -> usize {
        let before = self.sessions.len();
        self.sessions.retain(|_, s| s.expires_at > now);
        let _ = &self.log_level;
        before - self.sessions.len()
    }
}
