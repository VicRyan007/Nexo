use nexo_core::CallSignal;
use serde::{Deserialize, Serialize};

pub const SIGNAL_PROTOCOL: &str = "/nexo/call-signal/0.1.0";
// Keep room for CBOR and request metadata below the 512 KiB transport cap.
pub const MAX_SIGNALS_PER_REQUEST: usize = 12;
const MAX_TRACKED_DEVICE_KEYS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignalRequest {
    pub version: u8,
    pub device_key: [u8; 32],
    pub signals: Vec<CallSignal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignalResponse {
    pub version: u8,
    pub received: u16,
}

impl SignalRequest {
    #[must_use]
    pub fn new(device_key: [u8; 32], mut signals: Vec<CallSignal>) -> Self {
        signals.truncate(MAX_SIGNALS_PER_REQUEST);
        Self {
            version: 1,
            device_key,
            signals,
        }
    }

    #[must_use]
    pub fn is_within_limits(&self) -> bool {
        self.version == 1 && self.signals.len() <= MAX_SIGNALS_PER_REQUEST
    }
}

impl SignalResponse {
    #[must_use]
    pub fn received(count: usize) -> Self {
        Self {
            version: 1,
            received: u16::try_from(count.min(MAX_SIGNALS_PER_REQUEST)).unwrap_or_default(),
        }
    }
}

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Sliding window rate limiter to mitigate signaling floods / `DoS` attacks.
#[derive(Debug)]
pub struct SignalRateLimiter {
    max_requests: usize,
    window: Duration,
    history: HashMap<[u8; 32], Vec<Instant>>,
}

impl Default for SignalRateLimiter {
    fn default() -> Self {
        Self::new(30, Duration::from_secs(5))
    }
}

impl SignalRateLimiter {
    #[must_use]
    pub fn new(max_requests: usize, window: Duration) -> Self {
        Self {
            max_requests,
            window,
            history: HashMap::new(),
        }
    }

    /// Check if a request from `device_key` is allowed and record the attempt.
    pub fn check_and_record(&mut self, device_key: &[u8; 32], now: Instant) -> bool {
        self.prune_idle(now);
        if !self.history.contains_key(device_key)
            && self.history.len() >= MAX_TRACKED_DEVICE_KEYS
            && let Some((oldest_key, _)) = self
                .history
                .iter()
                .filter_map(|(key, entries)| entries.last().map(|last| (*key, *last)))
                .min_by_key(|(_, last)| *last)
        {
            self.history.remove(&oldest_key);
        }
        let entries = self.history.entry(*device_key).or_default();
        entries.retain(|t| now.duration_since(*t) <= self.window);
        if entries.len() >= self.max_requests {
            false
        } else {
            entries.push(now);
            true
        }
    }

    /// Prunes idle device keys to avoid memory growth.
    pub fn prune_idle(&mut self, now: Instant) {
        self.history.retain(|_, entries| {
            entries.retain(|t| now.duration_since(*t) <= self.window);
            !entries.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_bounds_bursts_and_recovers_after_window() {
        let mut limiter = SignalRateLimiter::new(3, Duration::from_millis(100));
        let device = [42u8; 32];
        let now = Instant::now();

        assert!(limiter.check_and_record(&device, now));
        assert!(limiter.check_and_record(&device, now));
        assert!(limiter.check_and_record(&device, now));
        // 4th request in the same window must be rejected
        assert!(!limiter.check_and_record(&device, now));

        // After window expires, new requests are accepted
        let later = now + Duration::from_millis(150);
        assert!(limiter.check_and_record(&device, later));

        limiter.prune_idle(later + Duration::from_millis(200));
        assert!(limiter.history.is_empty());
    }

    #[test]
    fn rate_limiter_caps_stale_device_keys() {
        let mut limiter = SignalRateLimiter::default();
        let now = Instant::now();
        for index in 0..(MAX_TRACKED_DEVICE_KEYS + 8) {
            let mut device = [0_u8; 32];
            device[..8].copy_from_slice(&(index as u64).to_le_bytes());
            assert!(limiter.check_and_record(&device, now));
        }
        assert!(limiter.history.len() <= MAX_TRACKED_DEVICE_KEYS);
    }
}
