use std::net::IpAddr;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use governor::clock::DefaultClock;
use governor::state::keyed::DashMapStateStore;
use governor::{Quota, RateLimiter};
use tracing::{debug, info};

use crate::config::RateLimitConfig;

pub type KeyedLimiter = RateLimiter<IpAddr, DashMapStateStore<IpAddr>, DefaultClock>;

/// Build a per-IP GCRA rate limiter from config.
pub fn build_limiter(cfg: &RateLimitConfig) -> Arc<KeyedLimiter> {
    let rps =
        NonZeroU32::new(cfg.requests_per_second).expect("requests_per_second validated > 0");
    let burst = NonZeroU32::new(cfg.burst_size).expect("burst_size validated > 0");

    let quota = Quota::per_second(rps).allow_burst(burst);
    Arc::new(RateLimiter::dashmap(quota))
}

/// Spawn a background task that periodically cleans up expired entries.
pub fn spawn_cleanup(limiter: Arc<KeyedLimiter>, interval_secs: u64) {
    let interval = Duration::from_secs(interval_secs);

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;

            let before = limiter.len();
            limiter.retain_recent();
            limiter.shrink_to_fit();
            let after = limiter.len();

            debug!(before, after, "rate limiter cleanup completed");
        }
    });

    info!(interval_secs, "rate limiter cleanup task started");
}
