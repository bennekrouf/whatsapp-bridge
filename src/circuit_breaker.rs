// src/circuit_breaker.rs
//
// Simple circuit breaker for external API calls (primarily Claude).
//
// States:
//   Closed  — requests pass through normally
//   Open    — requests fail immediately with a canned error
//   HalfOpen — one probe request is allowed; success → Closed, failure → Open
//
// Config via env vars:
//   CIRCUIT_BREAKER_THRESHOLD  — failures to trip (default 5)
//   CIRCUIT_BREAKER_TIMEOUT_S  — seconds before half-open probe (default 30)

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_THRESHOLD: u32 = 5;
const DEFAULT_TIMEOUT: u64 = 30;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

pub struct CircuitBreaker {
    failure_count: AtomicU32,
    threshold: u32,
    timeout: Duration,
    /// Epoch seconds when the circuit was opened
    opened_at: AtomicU64,
    /// 0 = Closed, 1 = Open, 2 = HalfOpen
    state: AtomicU32,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        let threshold = std::env::var("CIRCUIT_BREAKER_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_THRESHOLD);

        let timeout_s = std::env::var("CIRCUIT_BREAKER_TIMEOUT_S")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TIMEOUT);

        Self {
            failure_count: AtomicU32::new(0),
            threshold,
            timeout: Duration::from_secs(timeout_s),
            opened_at: AtomicU64::new(0),
            state: AtomicU32::new(0), // Closed
        }
    }

    fn now_epoch() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    pub fn state(&self) -> CircuitState {
        match self.state.load(Ordering::Relaxed) {
            1 => {
                // Check if timeout has elapsed → transition to HalfOpen
                let opened = self.opened_at.load(Ordering::Relaxed);
                if Self::now_epoch().saturating_sub(opened) >= self.timeout.as_secs() {
                    self.state.store(2, Ordering::Relaxed); // HalfOpen
                    CircuitState::HalfOpen
                } else {
                    CircuitState::Open
                }
            }
            2 => CircuitState::HalfOpen,
            _ => CircuitState::Closed,
        }
    }

    /// Check if a request is allowed. Returns false if circuit is Open.
    pub fn allow_request(&self) -> bool {
        match self.state() {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => true, // Allow one probe
            CircuitState::Open => false,
        }
    }

    /// Record a successful call — resets the circuit to Closed.
    pub fn record_success(&self) {
        self.failure_count.store(0, Ordering::Relaxed);
        self.state.store(0, Ordering::Relaxed); // Closed
    }

    /// Record a failed call — may trip the circuit to Open.
    pub fn record_failure(&self) {
        let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;

        // If in HalfOpen and the probe failed → back to Open
        if self.state.load(Ordering::Relaxed) == 2 {
            self.state.store(1, Ordering::Relaxed); // Open
            self.opened_at.store(Self::now_epoch(), Ordering::Relaxed);
            return;
        }

        // Trip to Open if threshold reached
        if count >= self.threshold {
            self.state.store(1, Ordering::Relaxed); // Open
            self.opened_at.store(Self::now_epoch(), Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_starts_closed() {
        let cb = CircuitBreaker::new();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request());
    }

    #[test]
    fn test_trips_after_threshold() {
        // Use default threshold of 5
        let cb = CircuitBreaker {
            failure_count: AtomicU32::new(0),
            threshold: 3,
            timeout: Duration::from_secs(30),
            opened_at: AtomicU64::new(0),
            state: AtomicU32::new(0),
        };

        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state(), CircuitState::Closed);

        cb.record_failure(); // 3rd failure → trips
        assert_eq!(cb.state(), CircuitState::Open);
        assert!(!cb.allow_request());
    }

    #[test]
    fn test_resets_on_success() {
        let cb = CircuitBreaker {
            failure_count: AtomicU32::new(0),
            threshold: 2,
            timeout: Duration::from_secs(0), // instant timeout for test
            opened_at: AtomicU64::new(0),
            state: AtomicU32::new(0),
        };

        cb.record_failure();
        cb.record_failure(); // trips

        // With timeout=0, state() immediately transitions Open → HalfOpen
        assert_eq!(cb.state(), CircuitState::HalfOpen);
        assert!(cb.allow_request());

        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
    }
}
