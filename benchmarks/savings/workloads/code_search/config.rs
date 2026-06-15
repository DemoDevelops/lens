//! Service configuration loaded from the environment.

pub struct Config {
    pub log_level: String,
    pub db_url: String,
    pub cache_ttl: u64,
    pub connect_timeout: u64,
    pub retry_limit: u32,
    pub auth_secret: String,
}

impl Config {
    pub fn from_env() -> Config {
        Config {
            log_level: env_or("LOG_LEVEL", "info"),
            db_url: env_or("DB_URL", "postgres://localhost/app"),
            cache_ttl: env_or("CACHE_TTL", "300").parse().unwrap_or(300),
            connect_timeout: env_or("CONNECT_TIMEOUT", "5").parse().unwrap_or(5),
            retry_limit: env_or("RETRY_LIMIT", "3").parse().unwrap_or(3),
            auth_secret: env_or("AUTH_SECRET", "change-me"),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.auth_secret == "change-me" {
            return Err("auth_secret must be set in config".to_string());
        }
        if self.retry_limit == 0 {
            return Err("retry_limit config must be > 0".to_string());
        }
        Ok(())
    }
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}
