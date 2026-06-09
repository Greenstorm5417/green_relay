//! Property-based test for the API-key auth lockout predicate (Property 14).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/auth.rs`) per the spec's test-placement note, and exercises the pure
//! lockout predicate (`lockout_until` / `is_locked_out`) of the
//! `green_relay` library against an independent oracle derived directly
//! from the acceptance criterion (Req 3.8).

use std::time::{Duration, Instant};

use proptest::prelude::*;
use green_relay::auth::{
    LOCKOUT_DURATION, LOCKOUT_FAILURE_THRESHOLD, LOCKOUT_WINDOW, is_locked_out, lockout_until,
};

/// Window length, in seconds, over which failures are counted (Req 3.8: 60 s).
const WINDOW_SECS: u64 = 60;
/// Lockout duration, in seconds, once triggered (Req 3.8: 300 s).
const LOCKOUT_SECS: u64 = 300;
/// Failure count within the trailing window that triggers a lockout (Req 3.8: 5).
const THRESHOLD: usize = 5;

/// Independent oracle, expressed in integer seconds, that re-derives the
/// lockout semantics straight from the criterion rather than from the
/// implementation: for every failure used as the trailing-window anchor, count
/// the failures lying within `[anchor - 60s, anchor]`; whenever that count
/// reaches the threshold the lockout is triggered at the anchor and stays in
/// effect until `anchor + 300s`. The latest such expiry governs.
///
/// Returns the latest lockout-expiry offset (in seconds), or `None` if no
/// trailing 60-second window ever reached the threshold.
fn oracle_lockout_until_secs(failure_offsets: &[u64]) -> Option<u64> {
    let mut sorted = failure_offsets.to_vec();
    sorted.sort_unstable();

    let mut latest_expiry: Option<u64> = None;
    for i in 0..sorted.len() {
        let anchor = sorted[i];
        let count = sorted[..=i]
            .iter()
            .filter(|&&t| anchor - t <= WINDOW_SECS)
            .count();
        if count >= THRESHOLD {
            let expiry = anchor + LOCKOUT_SECS;
            latest_expiry = Some(latest_expiry.map_or(expiry, |e| e.max(expiry)));
        }
    }
    latest_expiry
}

/// A timeline of failure offsets (seconds since a common base) plus a query
/// offset at which the lockout state is evaluated. The query is constrained to
/// be at or after every failure, mirroring real usage where the lockout is
/// checked at the current time and the recorded failures are all in the past.
fn timeline_strategy() -> impl Strategy<Value = (Vec<u64>, u64)> {
    // Inter-arrival gaps in 0..=25 s keep failures close enough that bursts
    // routinely reach the 5-in-60-s threshold while still producing plenty of
    // never-locked timelines for the negative side of the iff.
    proptest::collection::vec(0u64..=25, 0..=12).prop_flat_map(|gaps| {
        let mut offsets = Vec::with_capacity(gaps.len());
        let mut acc = 0u64;
        for g in &gaps {
            acc += *g;
            offsets.push(acc);
        }
        let last = offsets.last().copied().unwrap_or(0);
        // Query anywhere from the last failure up to well beyond the 300 s
        // lockout horizon so the expiry boundary is exercised in both
        // directions.
        (Just(offsets), last..=(last + LOCKOUT_SECS + 120))
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 14: API-key auth lockout predicate.
    // For any timeline of authentication attempts for a single key identifier,
    // the identifier is locked out if and only if 5 or more failures occurred
    // within any trailing 60-second window, and once triggered the lockout
    // remains in effect for 300 seconds.
    //
    // Validates: Requirements 3.8
    #[test]
    fn prop_auth_lockout_predicate((offsets, query) in timeline_strategy()) {
        let base = Instant::now();
        let failures: Vec<Instant> = offsets
            .iter()
            .map(|s| base + Duration::from_secs(*s))
            .collect();
        let now = base + Duration::from_secs(query);

        // Oracle: locked iff some trailing 60-s window reached the threshold
        // and the resulting (latest) lockout has not yet expired at `query`.
        let oracle_expiry = oracle_lockout_until_secs(&offsets);
        let oracle_locked = oracle_expiry.is_some_and(|e| query < e);

        // The implementation's lockout decision must match the oracle.
        prop_assert_eq!(
            is_locked_out(&failures, now),
            oracle_locked,
            "lockout decision disagreed with oracle: offsets={:?}, query={}",
            offsets,
            query
        );

        // The implementation's computed expiry must agree with the oracle's,
        // anchoring the "remains in effect for 300 s" half of the property.
        let impl_expiry = lockout_until(&failures);
        match (impl_expiry, oracle_expiry) {
            (Some(until), Some(expected)) => {
                let expected_instant = base + Duration::from_secs(expected);
                prop_assert_eq!(
                    until,
                    expected_instant,
                    "lockout expiry disagreed: offsets={:?}",
                    offsets
                );
            }
            (None, None) => {}
            (got, want) => prop_assert!(
                false,
                "lockout_until presence mismatch: got_some={}, want_some={}, offsets={:?}",
                got.is_some(),
                want.is_some(),
                offsets
            ),
        }
    }

    // Feature: sms-microservice, Property 14: API-key auth lockout predicate.
    // Boundary half of the predicate: a freshly triggered lockout is in effect
    // from the trigger up to but not including trigger + 300 s. With exactly
    // `THRESHOLD` failures inside one window, the lockout is active just before
    // the 300 s expiry and clear at/after it.
    //
    // Validates: Requirements 3.8
    #[test]
    fn prop_lockout_expiry_boundary(spacing in 0u64..=12, after in 0u64..=600) {
        // Exactly THRESHOLD failures spaced so the whole burst fits inside the
        // 60-s window (spacing * (THRESHOLD - 1) <= 60), guaranteeing a trigger
        // at the final failure.
        let base = Instant::now();
        let trigger_offset = spacing * (THRESHOLD as u64 - 1);
        prop_assume!(trigger_offset <= WINDOW_SECS);

        let failures: Vec<Instant> = (0..THRESHOLD)
            .map(|i| base + Duration::from_secs(spacing * i as u64))
            .collect();

        let trigger = base + Duration::from_secs(trigger_offset);
        let expected_until = trigger + LOCKOUT_DURATION;
        prop_assert_eq!(lockout_until(&failures), Some(expected_until));

        // Strictly before expiry => locked; at or after expiry => clear.
        let query = trigger + Duration::from_secs(after);
        prop_assert_eq!(is_locked_out(&failures, query), query < expected_until);
    }
}

#[test]
fn lockout_constants_match_requirement_3_8() {
    // Sanity-check the constants the property relies on so a future tweak to
    // the requirement surfaces here rather than as a confusing property failure.
    assert_eq!(LOCKOUT_FAILURE_THRESHOLD, THRESHOLD);
    assert_eq!(LOCKOUT_WINDOW, Duration::from_secs(WINDOW_SECS));
    assert_eq!(LOCKOUT_DURATION, Duration::from_secs(LOCKOUT_SECS));
}
