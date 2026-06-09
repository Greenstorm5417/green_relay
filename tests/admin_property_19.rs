//! Property-based test for Argon2 password round-trip (Property 19).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/admin.rs`) per the spec's test-placement note, and exercises the
//! public `hash_password` / `verify_password` functions of the
//! `sms_micro_service` library.
//!
//! Argon2 is intentionally expensive, so the case count is kept near the
//! 100-iteration minimum.

use proptest::prelude::*;
use sms_micro_service::admin::{hash_password, verify_password};

/// Generate an arbitrary password string, including the empty string and
/// strings containing arbitrary unicode, so the property holds across the
/// whole input space rather than just ASCII.
fn any_password() -> impl Strategy<Value = String> {
    ".{0,64}"
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 100, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 19: Argon2 password round-trip.
    // For any password `p` and any different password `q`, verifying `p`
    // against `hash_password(p)` succeeds, verifying `q` against
    // `hash_password(p)` fails, and the produced hash is never equal to the
    // plaintext password.
    //
    // Validates: Requirements 5.2, 5.3
    #[test]
    fn prop_argon2_password_round_trip(p in any_password(), q in any_password()) {
        // Constrain the pair so `q` is genuinely a *different* password than
        // `p`; equal samples are discarded rather than asserted upon.
        prop_assume!(p != q);

        let hash = hash_password(&p);

        // 1. The correct password verifies against its own hash (Req 5.3).
        prop_assert!(
            verify_password(&p, &hash),
            "the correct password must verify against its own hash"
        );

        // 2. A different password must not verify against `p`'s hash (Req 5.3).
        prop_assert!(
            !verify_password(&q, &hash),
            "a different password ({:?}) must not verify against the hash of {:?}",
            q,
            p
        );

        // 3. The produced hash is never equal to the plaintext password
        //    (Req 5.2 — passwords are stored as Argon2 hashes, not plaintext).
        prop_assert_ne!(
            &hash,
            &p,
            "the Argon2 hash must never equal the plaintext password"
        );
    }
}
