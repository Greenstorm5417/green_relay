//! Per-API-key fixed-window rate limiter core.
//!
//! This module holds the pure decision logic for the fixed-window rate
//! limiter (task 9.1). A [`WindowState`] tracks the request count and the
//! start of the current window for a single key; [`decide`] is the pure
//! transition function that resets the window when it elapses, allows and
//! counts requests under the configured limit, and rejects requests at or
//! over the limit without changing the count. [`effective_limit`] resolves a
//! key's effective limit, validating any custom override.
//!
//! The pure core is wrapped by [`RateLimiter`], a small in-memory map that
//! keeps an independent [`WindowState`] per key so that activity on one key
//! never affects another (Req 4.4).
//!
//! Validates: Requirements 4.1, 4.2, 4.3, 4.4, 4.5, 4.6, 4.7

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Lowest custom rate limit a key may configure (Req 4.6).
pub const CUSTOM_LIMIT_MIN: u32 = 1;

/// Highest custom rate limit a key may configure (Req 4.6).
pub const CUSTOM_LIMIT_MAX: u32 = 10_000;

/// Per-key fixed-window accounting state.
///
/// `count` is the number of requests allowed in the current window so far,
/// and `window_start` marks when the current window began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    /// Requests allowed in the current window so far.
    pub count: u32,
    /// Instant at which the current window began.
    pub window_start: Instant,
}

impl WindowState {
    /// Create a fresh state whose window begins at `now` with a zero count.
    pub fn new(now: Instant) -> Self {
        WindowState {
            count: 0,
            window_start: now,
        }
    }
}

/// Outcome of a rate-limit decision for a single request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    /// The request is permitted. `remaining` is the number of further
    /// requests still allowed in the current window after this one.
    Allow {
        /// Further requests allowed in the current window after this one.
        remaining: u32,
    },
    /// The request is rejected. `retry_after_secs` is the number of seconds
    /// until the window resets, always in `1..=window_secs` (Req 4.3).
    Reject {
        /// Seconds until requests are permitted again (1..=window length).
        retry_after_secs: u64,
    },
}

/// Error produced when a key's configured custom rate limit is out of range.
///
/// Surfaced by [`effective_limit`] when a custom limit falls outside
/// `1..=10_000`; the default limit is applied instead (Req 4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfigError {
    /// The out-of-range custom limit that was configured.
    pub configured: u32,
}

impl core::fmt::Display for RateLimitConfigError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "custom rate limit {} is out of the allowed range {}..={}",
            self.configured, CUSTOM_LIMIT_MIN, CUSTOM_LIMIT_MAX
        )
    }
}

impl std::error::Error for RateLimitConfigError {}

/// Pure fixed-window decision function.
///
/// Given the current `state`, the effective `limit`, the `window` length, and
/// the current instant `now`, this returns the decision and mutates `state`
/// accordingly:
///
/// - If the window has elapsed (`now - window_start >= window`), the window is
///   reset to start at `now`, this request is counted as the first of the new
///   window, and the request is allowed (Req 4.5, 4.1).
/// - Otherwise, while the count is below `limit`, the request is allowed and
///   the count is increased by exactly one (Req 4.1).
/// - Otherwise (count at or above `limit`), the request is rejected and the
///   count is left unchanged; the returned `retry_after_secs` is bounded to
///   `1..=window_secs` (Req 4.2, 4.3).
pub fn decide(state: &mut WindowState, limit: u32, window: Duration, now: Instant) -> RateDecision {
    let elapsed = now.saturating_duration_since(state.window_start);

    // Window elapsed: reset and count this request as the first of the new
    // window. The reset clears the prior count to zero before counting.
    if elapsed >= window {
        state.window_start = now;
        state.count = 1;
        return RateDecision::Allow {
            remaining: limit.saturating_sub(1),
        };
    }

    if state.count < limit {
        // Under the limit: allow and increment by exactly one.
        state.count = state.count.saturating_add(1);
        RateDecision::Allow {
            remaining: limit.saturating_sub(state.count),
        }
    } else {
        // At or over the limit: reject and leave the count unchanged.
        RateDecision::Reject {
            retry_after_secs: retry_after_secs(window, elapsed),
        }
    }
}

/// Compute the `Retry-After` value (in seconds) for a rejected request.
///
/// This is the time remaining until the current window ends, rounded up to a
/// whole second and clamped to `1..=window_secs` so it is always a positive
/// integer no greater than the configured window length (Req 4.3).
fn retry_after_secs(window: Duration, elapsed: Duration) -> u64 {
    // The integer window length in seconds, at least 1 so the upper bound is
    // never zero for any positive window.
    let window_secs = window.as_secs().max(1);
    let remaining = window.saturating_sub(elapsed);
    // Round up: any non-zero fraction of a second still requires waiting that
    // second out before the window resets.
    let secs = remaining.as_secs_f64().ceil() as u64;
    secs.clamp(1, window_secs)
}

/// Resolve the effective rate limit for a key.
///
/// When `custom` is `Some(c)` and `c` is within `1..=10_000`, the custom limit
/// is used with no error (Req 4.6). When `custom` is out of that range, the
/// `default` limit is used together with a [`RateLimitConfigError`] naming the
/// offending value (Req 4.7). When `custom` is `None`, the `default` limit is
/// used with no error.
pub fn effective_limit(custom: Option<u32>, default: u32) -> (u32, Option<RateLimitConfigError>) {
    match custom {
        Some(c) if (CUSTOM_LIMIT_MIN..=CUSTOM_LIMIT_MAX).contains(&c) => (c, None),
        Some(c) => (default, Some(RateLimitConfigError { configured: c })),
        None => (default, None),
    }
}

/// In-memory, per-key fixed-window rate limiter.
///
/// Maintains an independent [`WindowState`] for each key so that requests for
/// one key never change another key's accumulated count (Req 4.4). The actual
/// decision is delegated to the pure [`decide`] function.
#[derive(Debug, Default)]
pub struct RateLimiter {
    states: HashMap<String, WindowState>,
}

impl RateLimiter {
    /// Create an empty rate limiter with no tracked keys.
    pub fn new() -> Self {
        RateLimiter {
            states: HashMap::new(),
        }
    }

    /// Make a rate-limit decision for `key`, creating its window state on
    /// first use. The state for any other key is left untouched (Req 4.4).
    pub fn check(&mut self, key: &str, limit: u32, window: Duration, now: Instant) -> RateDecision {
        // Hot path: an already-tracked key is decided in place without
        // allocating a new owned key. Only the first request for a key pays
        // for the `String` allocation that inserts its window state.
        if let Some(state) = self.states.get_mut(key) {
            return decide(state, limit, window, now);
        }
        let mut state = WindowState::new(now);
        let decision = decide(&mut state, limit, window, now);
        self.states.insert(key.to_string(), state);
        decision
    }

    /// Current accumulated request count for `key`, or `0` if the key has not
    /// been seen. Primarily useful for asserting per-key isolation.
    pub fn count_for(&self, key: &str) -> u32 {
        self.states.get(key).map(|s| s.count).unwrap_or(0)
    }

    /// Borrow the [`WindowState`] tracked for `key`, if any.
    pub fn state_for(&self, key: &str) -> Option<&WindowState> {
        self.states.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_and_increments_under_limit() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut state = WindowState::new(now);

        assert_eq!(
            decide(&mut state, 3, window, now),
            RateDecision::Allow { remaining: 2 }
        );
        assert_eq!(state.count, 1);
        assert_eq!(
            decide(&mut state, 3, window, now),
            RateDecision::Allow { remaining: 1 }
        );
        assert_eq!(state.count, 2);
        assert_eq!(
            decide(&mut state, 3, window, now),
            RateDecision::Allow { remaining: 0 }
        );
        assert_eq!(state.count, 3);
    }

    #[test]
    fn rejects_at_or_over_limit_without_changing_count() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut state = WindowState {
            count: 3,
            window_start: now,
        };

        match decide(&mut state, 3, window, now) {
            RateDecision::Reject { retry_after_secs } => {
                assert!((1..=60).contains(&retry_after_secs));
            }
            other => panic!("expected reject, got {other:?}"),
        }
        // Count is unchanged after a rejection.
        assert_eq!(state.count, 3);
    }

    #[test]
    fn resets_window_after_it_elapses_and_allows() {
        let start = Instant::now();
        let window = Duration::from_secs(60);
        let mut state = WindowState {
            count: 100,
            window_start: start,
        };

        // Exactly at the window boundary: the window has elapsed and resets.
        let later = start + window;
        let decision = decide(&mut state, 100, window, later);
        assert_eq!(decision, RateDecision::Allow { remaining: 99 });
        assert_eq!(state.count, 1);
        assert_eq!(state.window_start, later);
    }

    #[test]
    fn retry_after_is_within_window_bound() {
        let start = Instant::now();
        let window = Duration::from_secs(60);
        // Limit reached just after the window opened.
        let mut state = WindowState {
            count: 1,
            window_start: start,
        };
        let now = start + Duration::from_secs(1);
        match decide(&mut state, 1, window, now) {
            RateDecision::Reject { retry_after_secs } => {
                // 59 seconds remain in the window.
                assert_eq!(retry_after_secs, 59);
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_never_below_one() {
        // Almost the whole window has elapsed but not quite; retry must still
        // be at least 1 second.
        let window = Duration::from_secs(60);
        let elapsed = window - Duration::from_millis(100);
        assert_eq!(retry_after_secs(window, elapsed), 1);
    }

    #[test]
    fn effective_limit_accepts_in_range_custom() {
        assert_eq!(effective_limit(Some(1), 100), (1, None));
        assert_eq!(effective_limit(Some(10_000), 100), (10_000, None));
        assert_eq!(effective_limit(Some(500), 100), (500, None));
    }

    #[test]
    fn effective_limit_falls_back_on_out_of_range_custom() {
        assert_eq!(
            effective_limit(Some(0), 100),
            (100, Some(RateLimitConfigError { configured: 0 }))
        );
        assert_eq!(
            effective_limit(Some(10_001), 100),
            (100, Some(RateLimitConfigError { configured: 10_001 }))
        );
    }

    #[test]
    fn effective_limit_uses_default_when_no_custom() {
        assert_eq!(effective_limit(None, 100), (100, None));
    }

    #[test]
    fn per_key_state_is_isolated() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let mut limiter = RateLimiter::new();

        // Drive key "a" several times.
        for _ in 0..5 {
            limiter.check("a", 100, window, now);
        }
        // Touch key "b" once.
        limiter.check("b", 100, window, now);

        assert_eq!(limiter.count_for("a"), 5);
        assert_eq!(limiter.count_for("b"), 1);
        // An untouched key has no accumulated count.
        assert_eq!(limiter.count_for("c"), 0);
    }
}
