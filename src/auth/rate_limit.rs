//! Per-client request rate limit for the public demo: a token bucket per client
//! address, refilled continuously. In-process, so it suits a single instance; behind a
//! load balancer, use nginx's `limit_req` instead (or as well).

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

pub struct RateLimiter {
    per_minute: u32,
    buckets: Mutex<HashMap<String, (f64, Instant)>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        RateLimiter { per_minute, buckets: Mutex::new(HashMap::new()) }
    }

    pub fn enabled(&self) -> bool {
        self.per_minute > 0
    }

    /// Takes `cost` tokens for `key`, or returns how many seconds to wait.
    pub fn check(&self, key: &str, cost: u32) -> Result<(), u64> {
        self.check_at(key, cost, Instant::now())
    }

    fn check_at(&self, key: &str, cost: u32, now: Instant) -> Result<(), u64> {
        if !self.enabled() {
            return Ok(());
        }
        let capacity = self.per_minute as f64;
        let per_second = capacity / 60.0;
        let mut buckets = self.buckets.lock().unwrap();
        let (tokens, last) = buckets.entry(key.to_string()).or_insert((capacity, now));
        *tokens = (*tokens + now.saturating_duration_since(*last).as_secs_f64() * per_second).min(capacity);
        *last = now;
        let cost = (cost as f64).min(capacity);
        if *tokens >= cost {
            *tokens -= cost;
            Ok(())
        } else {
            Err(((cost - *tokens) / per_second).ceil().max(1.0) as u64)
        }
    }

    /// Forgets clients idle long enough for their bucket to be full again.
    pub fn prune(&self) {
        let now = Instant::now();
        self.buckets.lock().unwrap().retain(|_, (_, last)| now.saturating_duration_since(*last) < Duration::from_secs(120));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_bursts_up_to_the_limit_then_refills() {
        let limiter = RateLimiter::new(60);
        let start = Instant::now();
        for _ in 0..60 {
            assert!(limiter.check_at("1.2.3.4", 1, start).is_ok());
        }
        assert_eq!(limiter.check_at("1.2.3.4", 1, start), Err(1));
        assert!(limiter.check_at("5.6.7.8", 1, start).is_ok(), "clients are limited separately");
        assert!(limiter.check_at("1.2.3.4", 1, start + Duration::from_secs(2)).is_ok());
        assert_eq!(limiter.check_at("9.9.9.9", 10, start), Ok(()));
        assert!(RateLimiter::new(0).check("x", 1000).is_ok(), "0 disables the limit");
    }
}
