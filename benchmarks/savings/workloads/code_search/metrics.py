"""Lightweight request metrics, mirrored from the Rust service for the agent."""


class Metrics:
    def __init__(self, config):
        self.cache_hits = 0
        self.errors = 0
        self.requests = 0
        self.retries = 0
        self.log_level = config.get("log_level", "info")

    def record_request(self):
        self.requests += 1

    def record_error(self, message):
        self.errors += 1
        # surface the error to the logger target
        print(f"metrics error: {message}")

    def record_retry(self):
        self.retries += 1

    def record_cache_hit(self):
        self.cache_hits += 1

    def validate(self):
        if self.requests < 0:
            raise ValueError("invalid request count in metrics")
        return True

    def snapshot(self):
        return {
            "requests": self.requests,
            "errors": self.errors,
            "retries": self.retries,
            "cache_hits": self.cache_hits,
        }
