// src/rate_limit.rs
//
// In-memory sliding-window rate limiter.
// Two limits enforced independently:
//   • Per phone number  — protects against a single spammy customer
//   • Per tenant        — protects against total cost runaway per tenant
//
// Expired entries are lazily pruned on each check.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

const DEFAULT_PHONE_LIMIT: u32 = 10;   // max messages per phone per window
const DEFAULT_TENANT_LIMIT: u32 = 100;  // max messages per tenant per window
const WINDOW: Duration = Duration::from_secs(60);

// Prune entries older than this to keep memory bounded
const PRUNE_THRESHOLD: usize = 5_000;

struct SlidingWindow {
    timestamps: Vec<Instant>,
}

impl SlidingWindow {
    fn new() -> Self {
        Self { timestamps: Vec::new() }
    }

    /// Remove timestamps outside the window. Returns current count.
    fn count_and_prune(&mut self, now: Instant) -> u32 {
        self.timestamps.retain(|t| now.duration_since(*t) < WINDOW);
        self.timestamps.len() as u32
    }

    fn record(&mut self, now: Instant) {
        self.timestamps.push(now);
    }
}

pub struct RateLimiter {
    phone_windows: Arc<Mutex<HashMap<String, SlidingWindow>>>,
    tenant_windows: Arc<Mutex<HashMap<String, SlidingWindow>>>,
    phone_limit: u32,
    tenant_limit: u32,
}

#[derive(Debug)]
pub enum RateLimitDenied {
    Phone,
    Tenant,
}

impl RateLimiter {
    pub fn new() -> Self {
        let phone_limit = std::env::var("WA_RATE_LIMIT_PHONE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_PHONE_LIMIT);

        let tenant_limit = std::env::var("WA_RATE_LIMIT_TENANT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TENANT_LIMIT);

        Self {
            phone_windows: Arc::new(Mutex::new(HashMap::new())),
            tenant_windows: Arc::new(Mutex::new(HashMap::new())),
            phone_limit,
            tenant_limit,
        }
    }

    /// Check both rate limits. If allowed, records the message and returns Ok(()).
    /// If denied, returns which limit was hit.
    pub async fn check_and_record(
        &self,
        tenant_id: &str,
        phone: &str,
    ) -> Result<(), RateLimitDenied> {
        let now = Instant::now();

        // Check per-phone limit
        {
            let mut phones = self.phone_windows.lock().await;
            let key = format!("{}:{}", tenant_id, phone);
            let window = phones.entry(key).or_insert_with(SlidingWindow::new);
            if window.count_and_prune(now) >= self.phone_limit {
                return Err(RateLimitDenied::Phone);
            }
            window.record(now);

            // Lazy prune: if map is too large, drop oldest entries
            if phones.len() > PRUNE_THRESHOLD {
                phones.retain(|_, w| {
                    w.count_and_prune(now);
                    !w.timestamps.is_empty()
                });
            }
        }

        // Check per-tenant limit
        {
            let mut tenants = self.tenant_windows.lock().await;
            let window = tenants
                .entry(tenant_id.to_string())
                .or_insert_with(SlidingWindow::new);
            if window.count_and_prune(now) >= self.tenant_limit {
                return Err(RateLimitDenied::Tenant);
            }
            window.record(now);
        }

        Ok(())
    }
}
