//! Property-based test for the dashboard's recent-activity selection.
//!
//! Feature: sms-microservice, Property 21: Recent-activity selection
//! Validates: Requirements 5.7
//!
//! Property 21 (from design.md): *For any* set of activity entries, the
//! dashboard selection returns at most 10 entries, every returned entry has a
//! timestamp within the preceding 24 hours, and the entries are ordered
//! most-recent-first.
//!
//! This lives in its own integration-test file (separate from `src/admin.rs`)
//! and exercises the public API only.

use chrono::{DateTime, Duration, TimeZone, Utc};
use proptest::prelude::*;
use sms_micro_service::admin::{ActivityEntry, RECENT_ACTIVITY_LIMIT, recent_activity};

/// A fixed reference "now" used as the selection cutoff. Using a fixed instant
/// (rather than the wall clock) keeps cases deterministic and reproducible.
fn reference_now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2024, 6, 1, 12, 0, 0).unwrap()
}

/// Generate an activity entry whose timestamp is offset from `now` by some
/// number of seconds in `[-window, +window]`. The range deliberately spans
/// well beyond the 24-hour window (and into the future) so the generator
/// exercises in-window, too-old, and future entries.
fn entry_strategy(now: DateTime<Utc>) -> impl Strategy<Value = ActivityEntry> {
    // +/- 48 hours, in seconds, comfortably straddles the 24-hour boundary.
    let bound = 48 * 60 * 60i64;
    ((-bound)..=bound, "\\PC{0,32}").prop_map(move |(offset_secs, description)| ActivityEntry {
        timestamp: now + Duration::seconds(offset_secs),
        description,
    })
}

fn entries_strategy(now: DateTime<Utc>) -> impl Strategy<Value = Vec<ActivityEntry>> {
    // 0..=30 entries so the cap of 10 is regularly exceeded.
    prop::collection::vec(entry_strategy(now), 0..=30)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 21: Recent-activity selection.
    //
    // Validates: Requirements 5.7
    #[test]
    fn prop_recent_activity_selection(entries in entries_strategy(reference_now())) {
        let now = reference_now();
        let window = Duration::hours(24);
        let selected = recent_activity(&entries, now);

        // (1) At most 10 entries are returned.
        prop_assert!(
            selected.len() <= RECENT_ACTIVITY_LIMIT,
            "selection returned {} entries, exceeding the limit of {}",
            selected.len(),
            RECENT_ACTIVITY_LIMIT
        );

        // (2) Every returned entry has a timestamp within the preceding 24 h:
        //     not in the future and no older than 24 hours.
        for entry in &selected {
            let age = now.signed_duration_since(entry.timestamp);
            prop_assert!(
                age >= Duration::zero(),
                "selected entry is in the future relative to now"
            );
            prop_assert!(
                age <= window,
                "selected entry is older than the 24-hour window"
            );
        }

        // (3) Entries are ordered most-recent-first (non-increasing timestamps).
        for pair in selected.windows(2) {
            prop_assert!(
                pair[0].timestamp >= pair[1].timestamp,
                "entries are not ordered most-recent-first"
            );
        }

        // (4) The selection is a faithful subset: it contains exactly the
        //     in-window entries, capped at 10. Cross-check the count against an
        //     independent computation of how many entries fall in the window.
        let in_window = entries
            .iter()
            .filter(|e| {
                let age = now.signed_duration_since(e.timestamp);
                age >= Duration::zero() && age <= window
            })
            .count();
        let expected_len = in_window.min(RECENT_ACTIVITY_LIMIT);
        prop_assert_eq!(selected.len(), expected_len);

        // Every selected entry is one of the in-window source entries.
        for entry in &selected {
            let age = now.signed_duration_since(entry.timestamp);
            prop_assert!(age >= Duration::zero() && age <= window);
            prop_assert!(
                entries.contains(entry),
                "selected entry was not present in the source set"
            );
        }
    }
}
