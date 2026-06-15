//! HTTP request handlers wiring auth, cache, and db together.

use crate::auth::Auth;
use crate::cache::Cache;
use crate::db::Db;
use crate::logger::Logger;

pub struct Handlers<'a> {
    auth: &'a Auth,
    cache: &'a mut Cache,
    db: &'a Db,
    logger: &'a Logger,
}

impl<'a> Handlers<'a> {
    pub fn handle_request(&mut self, token: &str, key: &str) -> Result<String, String> {
        self.logger.request("GET", key);
        self.auth.validate(token, self.logger)?;

        if let Some(cached) = self.cache.get(key) {
            self.logger.info("cache hit on request");
            return Ok(cached.clone());
        }

        let rows = self.db.query(key).map_err(|e| {
            self.logger.error(&format!("request db error: {}", e));
            e
        })?;
        let value = rows.join(",");
        self.cache.set(key, &value);
        Ok(value)
    }
}
