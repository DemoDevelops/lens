//! Payments integration: charge, refund, and reconciliation against an upstream
//! provider. Wraps the outbound client with retry and structured logging.

use crate::client::Client;
use crate::config::Config;
use crate::logger::Logger;
use crate::retry::{should_retry, with_retry};

pub struct Payments {
    client: Client,
    retry_limit: u32,
    provider: String,
}

#[derive(Debug)]
pub struct Charge {
    pub id: String,
    pub amount_cents: u64,
    pub currency: String,
    pub idempotency_key: String,
}

#[derive(Debug)]
pub struct Refund {
    pub charge_id: String,
    pub amount_cents: u64,
}

impl Payments {
    pub fn new(config: &Config) -> Payments {
        Payments {
            client: Client::new(config),
            retry_limit: config.retry_limit,
            provider: "payments-api".to_string(),
        }
    }

    pub fn charge(&self, charge: &Charge, logger: &Logger) -> Result<String, String> {
        logger.request("POST", "/charge");
        if charge.amount_cents == 0 {
            logger.error("payments error: zero amount charge rejected");
            return Err("payments: amount must be > 0".to_string());
        }
        if charge.idempotency_key.is_empty() {
            logger.error("payments error: missing idempotency_key on request");
            return Err("payments: idempotency_key required".to_string());
        }
        let url = format!("https://{}/charge/{}", self.provider, charge.id);
        with_retry(self.retry_limit, || {
            match self.client.fetch(&url, logger) {
                Ok(resp) => Ok(resp),
                Err(e) => {
                    if should_retry(&e) {
                        logger.warn("payments: retrying charge after transient error");
                    }
                    Err(format!("payments connect error: {}", e))
                }
            }
        })
    }

    pub fn refund(&self, refund: &Refund, logger: &Logger) -> Result<String, String> {
        logger.request("POST", "/refund");
        if refund.amount_cents == 0 {
            logger.error("payments error: zero amount refund rejected");
            return Err("payments: refund amount must be > 0".to_string());
        }
        let url = format!("https://{}/refund/{}", self.provider, refund.charge_id);
        self.client
            .fetch(&url, logger)
            .map_err(|e| format!("payments refund connect error: {}", e))
    }

    pub fn validate_charge(&self, charge: &Charge) -> Result<(), String> {
        if charge.currency.len() != 3 {
            return Err("payments validate: currency must be a 3-letter code".to_string());
        }
        if charge.id.is_empty() {
            return Err("payments validate: charge id required".to_string());
        }
        Ok(())
    }

    pub fn reconcile(&self, charges: &[Charge], logger: &Logger) -> u64 {
        let mut total = 0u64;
        for charge in charges {
            if self.validate_charge(charge).is_ok() {
                total += charge.amount_cents;
            } else {
                logger.warn("payments: skipping invalid charge during reconcile");
            }
        }
        logger.info("payments: reconcile complete");
        total
    }
}
