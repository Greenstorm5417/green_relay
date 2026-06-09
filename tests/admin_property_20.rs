//! Property-based test for the admin login lockout predicate (Property 20).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/admin.rs`) per the spec's test-placement note, and exercises the
//! public `admin_locked` function of the `sms_micro_service` library.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use sms_micro_service::admin::{
    ADMIN_FAILURE_WINDOW, ADMIN_LOCK_DURATION, ADMIN_MAX_FAILURES, admin_locked,
};

/// Seconds in the 15-minute failure window and lock duration (Req 5.5).
const WINDOW_SECS: u64 = 15 * 60;
const LOCK_SECS: u64 = 15 * 60;

/// Independent integer-arithmetic oracle for the lockout predicate.
///
/// Computed in whole seconds (rather than with `Instant`/`Duration`) so it
/// is structurally independent from the implementation under test. The
/// account is locked at `now` if and only if there exists a failure
/// `trigger` such that at least `ADMIN_MAX_FAILURES` failures lie within the
/// trailing 15-minute window ending at `trigger`, and `now` falls within the
/// 15-minute lock window `[trigger, trigger + 15m)` that the trigger opens.
fn oracle_locked(failure_secs: &[u64], now_secs: u64) -> bool {
    for &trigger in failure_secs {
        let count = failure_secs
            .iter()
            .filter(|&&f| f <= trigger && trigger - f <= WINDOW_SECS)
            .count();
        if count >= ADMIN_MAX_FAILURES && now_secs >= trigger && now_secs - trigger < LOCK_SECS {
            return true;
        }
    }
    false
}

/// Generate a timeline of 0..=12 failure offsets (in seconds) spread over a
/// window wide enough to exercise both inside-window and outside-window
/// groupings around the 900-second (15-minute) boundary.
fn any_timeline() -> impl Strategy<Value = Vec<u64>> {
    proptest::collection::vec(0u64..=3600, 0..=12)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 20: Admin login lockout predicate.
    // For any timeline of failed admin logins for a single account, the
    // account is locked if and only if 5 or more failures occurred within any
    // trailing 15-minute window, and once triggered the lock remains in
    // effect for 15 minutes.
    //
    // Validates: Requirements 5.5
    #[test]
    fn prop_admin_login_lockout_predicate(
        failure_secs in any_timeline(),
        now_secs in 0u64..=5400,
    ) {
        // Map the integer-second offsets onto real `Instant`s sharing a base.
        let base = Instant::now();
        let failures: Vec<Instant> = failure_secs
            .iter()
            .map(|&s| base + Duration::from_secs(s))
            .collect();
        let now = base + Duration::from_secs(now_secs);

        let actual = admin_locked(&failures, now);
        let expected = oracle_locked(&failure_secs, now_secs);

        // 1. The predicate matches the independent oracle exactly (the iff).
        prop_assert_eq!(
            actual,
            expected,
            "admin_locked disagreed with oracle: failures={:?}, now={}",
            failure_secs,
            now_secs
        );

        // 2. Necessary condition: a lock requires at least the threshold
        //    number of failures to exist at all.
        if actual {
            prop_assert!(
                failure_secs.len() >= ADMIN_MAX_FAILURES,
                "locked with fewer than {} failures",
                ADMIN_MAX_FAILURES
            );
        }
    }

    // Feature: sms-microservice, Property 20: Admin login lockout predicate.
    // Reinforces the "remains in effect for 15 minutes" clause: when a lock is
    // active, it stays active throughout the trailing portion of its window
    // and clears at the boundary.
    //
    // Validates: Requirements 5.5
    #[test]
    fn prop_lock_persists_for_full_duration(
        failure_secs in proptest::collection::vec(0u64..=600, 5..=10),
    ) {
        let base = Instant::now();
        let failures: Vec<Instant> = failure_secs
            .iter()
            .map(|&s| base + Duration::from_secs(s))
            .collect();

        // The latest failure is the earliest instant by which all generated
        // failures (>=5, all within a 600s < 900s window) have occurred, so a
        // lock is guaranteed to be active there.
        let last = *failure_secs.iter().max().unwrap();
        let trigger = base + Duration::from_secs(last);

        // Active at the trigger instant.
        prop_assert!(admin_locked(&failures, trigger));
        // Active just before the lock window closes.
        prop_assert!(admin_locked(
            &failures,
            trigger + ADMIN_LOCK_DURATION - Duration::from_secs(1)
        ));
        // Cleared exactly at the 15-minute boundary (and the failure window
        // is also 15 minutes, so no later trigger can re-arm it here).
        prop_assert_eq!(
            admin_locked(&failures, trigger + ADMIN_LOCK_DURATION),
            // A later failure could still hold the lock; recompute via oracle
            // in seconds to stay exact.
            oracle_locked(&failure_secs, last + LOCK_SECS)
        );

        // Sanity: the configured constants match the 15-minute spec values.
        prop_assert_eq!(ADMIN_FAILURE_WINDOW, Duration::from_secs(WINDOW_SECS));
        prop_assert_eq!(ADMIN_LOCK_DURATION, Duration::from_secs(LOCK_SECS));
    }
}
