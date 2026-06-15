//! Background job scheduler with retrying workers and a small cache of results.

use std::collections::VecDeque;

use crate::cache::Cache;
use crate::config::Config;
use crate::logger::Logger;
use crate::retry::with_retry;

pub struct Job {
    pub name: String,
    pub payload: String,
    pub attempts: u32,
}

pub struct Scheduler {
    queue: VecDeque<Job>,
    retry_limit: u32,
    log_level: String,
}

impl Scheduler {
    pub fn new(config: &Config) -> Scheduler {
        Scheduler {
            queue: VecDeque::new(),
            retry_limit: config.retry_limit,
            log_level: config.log_level.clone(),
        }
    }

    pub fn submit(&mut self, name: &str, payload: &str, logger: &Logger) {
        logger.info(&format!("scheduler: submit job {}", name));
        self.queue.push_back(Job {
            name: name.to_string(),
            payload: payload.to_string(),
            attempts: 0,
        });
    }

    pub fn run_next<F>(&mut self, logger: &Logger, mut worker: F) -> Option<Result<String, String>>
    where
        F: FnMut(&Job) -> Result<String, String>,
    {
        let job = self.queue.pop_front()?;
        logger.request("JOB", &job.name);
        let result = with_retry(self.retry_limit, || worker(&job));
        match &result {
            Ok(_) => logger.info(&format!("scheduler: job {} ok", job.name)),
            Err(e) => logger.error(&format!("scheduler error: job {} failed: {}", job.name, e)),
        }
        Some(result)
    }

    pub fn drain<F>(&mut self, logger: &Logger, mut worker: F) -> Vec<Result<String, String>>
    where
        F: FnMut(&Job) -> Result<String, String>,
    {
        let mut out = Vec::new();
        while let Some(r) = self.run_next(logger, &mut worker) {
            out.push(r);
        }
        out
    }

    pub fn cache_results(&self, cache: &mut Cache, results: &[(String, String)]) {
        for (key, value) in results {
            cache.set(key, value);
        }
    }

    pub fn pending(&self) -> usize {
        self.queue.len()
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.retry_limit == 0 {
            return Err("scheduler config invalid: retry_limit is 0".to_string());
        }
        let _ = &self.log_level;
        Ok(())
    }
}
