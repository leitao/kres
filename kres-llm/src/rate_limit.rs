//! Shared rate limiter — currently a no-op.
//!
//! disabled its client-side pacer after finding
//! it undercounted: it reserved against a chars/4 estimate without
//! crediting `cache_read_input_tokens` / `cache_creation_input_tokens`
//! from the server-side accounting. kres has followed suit: the
//! server is the source of truth on rate budgets, and the Client's
//! 429 + retry-after handling enforces backoff when the budget is
//! hit. `reserve()` therefore returns immediately.
//!
//! The type is preserved so existing plumbing (agents construct
//! `Option<Arc<RateLimiter>>` via `new`, clients hold one via
//! `with_rate_limiter`) keeps compiling. When the time comes to put
//! real pacing back, this is the place.

use std::sync::Arc;

pub struct RateLimiter {
    _capacity: u64,
}

impl RateLimiter {
    /// Construct a limiter. Returns None when `capacity == 0` so
    /// callers can use `Option<Arc<...>>` interchangeably with
    /// "no limit". The capacity is retained only for introspection —
    /// the limiter itself never blocks.
    pub fn new(capacity: u64) -> Option<Arc<Self>> {
        if capacity == 0 {
            return None;
        }
        Some(Arc::new(Self {
            _capacity: capacity,
        }))
    }

    /// No-op. See module docs: kres relies on server-side 429s for
    /// pacing, not client-side reservations.
    pub async fn reserve(&self, _weight: u64) {}

    /// Always 0 — the limiter tracks nothing.
    pub fn in_flight(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reserve_is_noop() {
        let r = RateLimiter::new(1000).unwrap();
        r.reserve(100).await;
        r.reserve(999_999_999).await;
        // Never blocks, always reports zero.
        assert_eq!(r.in_flight(), 0);
    }

    #[test]
    fn zero_capacity_returns_none() {
        assert!(RateLimiter::new(0).is_none());
    }
}
