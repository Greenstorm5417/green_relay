use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const CUSTOM_LIMIT_MIN: u32 = 1;

pub const CUSTOM_LIMIT_MAX: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    pub count: u32,

    pub window_start: Instant,
}

impl WindowState {
    pub fn new(now: Instant) -> Self {
        WindowState {
            count: 0,
            window_start: now,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateDecision {
    Allow { remaining: u32 },

    Reject { retry_after_secs: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimitConfigError {
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

pub fn decide(state: &mut WindowState, limit: u32, window: Duration, now: Instant) -> RateDecision {
    let elapsed = now.saturating_duration_since(state.window_start);

    if elapsed >= window {
        state.window_start = now;
        state.count = 1;
        return RateDecision::Allow {
            remaining: limit.saturating_sub(1),
        };
    }

    if state.count < limit {
        state.count = state.count.saturating_add(1);
        RateDecision::Allow {
            remaining: limit.saturating_sub(state.count),
        }
    } else {
        RateDecision::Reject {
            retry_after_secs: retry_after_secs(window, elapsed),
        }
    }
}

fn retry_after_secs(window: Duration, elapsed: Duration) -> u64 {
    let window_secs = window.as_secs().max(1);
    let remaining = window.saturating_sub(elapsed);
    let secs = remaining.as_secs_f64().ceil() as u64;
    secs.clamp(1, window_secs)
}

pub fn effective_limit(custom: Option<u32>, default: u32) -> (u32, Option<RateLimitConfigError>) {
    match custom {
        Some(c) if (CUSTOM_LIMIT_MIN..=CUSTOM_LIMIT_MAX).contains(&c) => (c, None),
        Some(c) => (default, Some(RateLimitConfigError { configured: c })),
        None => (default, None),
    }
}

/// Maximum number of distinct keys retained. `moka` evicts least-recently-used
/// entries beyond this cap and idle entries past the TTL, so no manual sweeping
/// is required.
const MAX_TRACKED_KEYS: u64 = 50_000;

/// Idle entries are dropped after this long — far longer than any sane rate
/// window, so eviction only ever reclaims memory, never affects a decision.
const ENTRY_TTL: Duration = Duration::from_secs(3600);

/// Per-key sliding-window rate limiter backed by a bounded `moka` cache.
///
/// `moka` owns capacity bounding and TTL eviction; each entry is a small
/// `WindowState` behind a lightweight mutex so the read-modify-write of a single
/// key's counter stays atomic. Cloning is cheap (the cache is internally
/// reference-counted).
#[derive(Clone)]
pub struct RateLimiter {
    states: moka::future::Cache<String, Arc<Mutex<WindowState>>>,
}

impl RateLimiter {
    /// Creates a new, empty rate limiter.
    pub fn new() -> Self {
        RateLimiter {
            states: moka::future::Cache::builder()
                .max_capacity(MAX_TRACKED_KEYS)
                .time_to_idle(ENTRY_TTL)
                .build(),
        }
    }

    /// Records a request for `key` and returns the limiter decision.
    pub async fn check(
        &self,
        key: &str,
        limit: u32,
        window: Duration,
        now: Instant,
    ) -> RateDecision {
        let cell = self
            .states
            .get_with(key.to_string(), async {
                Arc::new(Mutex::new(WindowState::new(now)))
            })
            .await;
        let mut state = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        decide(&mut state, limit, window, now)
    }

    /// Returns the current request count for `key` (0 if untracked).
    pub async fn count_for(&self, key: &str) -> u32 {
        match self.states.get(key).await {
            Some(cell) => {
                cell.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .count
            }
            None => 0,
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        RateLimiter::new()
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

        let mut state = WindowState {
            count: 1,
            window_start: start,
        };
        let now = start + Duration::from_secs(1);
        match decide(&mut state, 1, window, now) {
            RateDecision::Reject { retry_after_secs } => {
                assert_eq!(retry_after_secs, 59);
            }
            other => panic!("expected reject, got {other:?}"),
        }
    }

    #[test]
    fn retry_after_never_below_one() {
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

    #[tokio::test]
    async fn per_key_state_is_isolated() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let limiter = RateLimiter::new();

        for _ in 0..5 {
            limiter.check("a", 100, window, now).await;
        }

        limiter.check("b", 100, window, now).await;

        assert_eq!(limiter.count_for("a").await, 5);
        assert_eq!(limiter.count_for("b").await, 1);
        assert_eq!(limiter.count_for("c").await, 0);
    }

    #[tokio::test]
    async fn check_rejects_once_limit_is_reached() {
        let now = Instant::now();
        let window = Duration::from_secs(60);
        let limiter = RateLimiter::new();

        assert!(matches!(
            limiter.check("k", 1, window, now).await,
            RateDecision::Allow { .. }
        ));
        assert!(matches!(
            limiter.check("k", 1, window, now).await,
            RateDecision::Reject { .. }
        ));
    }
}
