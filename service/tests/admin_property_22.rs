//! Property-based test for session validity boundary (Property 22).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/admin.rs`) per the spec's test-placement note, and exercises the
//! public `session_valid` function of the `green_relay` library.

use std::time::{Duration, Instant};

use green_relay::admin::{SESSION_IDLE_TIMEOUT, Session, session_valid};
use proptest::prelude::*;

/// The idle timeout, in milliseconds, used as the validity boundary.
const TIMEOUT_MS: u64 = SESSION_IDLE_TIMEOUT.as_millis() as u64;

/// Generate an elapsed duration (in milliseconds) since a session's last
/// activity. The range spans well below and well above the 30-minute boundary
/// (0 to 1 hour) so both the "valid" and "expired" sides are exercised, while
/// `boundary_skewed_elapsed_ms` concentrates extra cases right at the edge.
fn any_elapsed_ms() -> impl Strategy<Value = u64> {
    prop_oneof![
        // Broad coverage across the whole 0..=1h range.
        0u64..=(2 * TIMEOUT_MS),
        // Extra density within a few seconds of the 30-minute boundary so the
        // strict-less-than edge is thoroughly probed.
        (TIMEOUT_MS.saturating_sub(5_000))..=(TIMEOUT_MS + 5_000),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 22: Session validity boundary.
    // For any session and current time, the session is valid if and only if
    // the elapsed time since its last activity is strictly less than 30
    // minutes.
    //
    // Validates: Requirements 5.8, 5.9
    #[test]
    fn prop_session_validity_boundary(elapsed_ms in any_elapsed_ms()) {
        // Build a `now` that is exactly `elapsed_ms` after the session's last
        // activity. Anchoring `last_activity` first and adding the elapsed
        // duration avoids any `Instant` underflow.
        let last_activity = Instant::now();
        let elapsed = Duration::from_millis(elapsed_ms);
        let now = last_activity
            .checked_add(elapsed)
            .expect("instant + elapsed must not overflow");

        let session = Session {
            admin_id: 1,
            last_activity,
            csrf_token: "csrf".to_string(),
        };

        let expected_valid = elapsed < SESSION_IDLE_TIMEOUT;
        let actual_valid = session_valid(&session, now);

        prop_assert_eq!(
            actual_valid,
            expected_valid,
            "session with {}ms idle: expected valid={} (boundary={}ms), got {}",
            elapsed_ms,
            expected_valid,
            TIMEOUT_MS,
            actual_valid
        );
    }
}
