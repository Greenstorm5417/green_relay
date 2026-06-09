//! Property-based test for API key hashing (Property 11).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/auth.rs`) per the spec's test-placement note, and exercises the public
//! `key_identifier` function of the `green_relay` library.

use std::collections::HashMap;

use proptest::prelude::*;
use green_relay::auth::key_identifier;

/// Expected length, in characters, of a SHA-256 hex identifier.
const SHA256_HEX_LEN: usize = 64;

proptest! {
    #![proptest_config(ProptestConfig { cases: 1000, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 11: API key hashing is deterministic
    // and non-reversible. For any API key string, `key_identifier` is
    // deterministic (the same key always hashes to the same value), its output
    // is never equal to the plaintext key and has a fixed length, and distinct
    // keys produce distinct identifiers.
    //
    // Validates: Requirements 3.5
    #[test]
    fn prop_key_hashing_deterministic_and_non_reversible(
        key in any::<String>(),
        other in any::<String>(),
    ) {
        let id = key_identifier(&key);

        // Deterministic: hashing the same key again yields the same value.
        prop_assert_eq!(
            &id,
            &key_identifier(&key),
            "key_identifier({:?}) was not deterministic",
            key
        );

        // Fixed length: a SHA-256 hex identifier is always 64 lowercase hex
        // characters regardless of input length.
        prop_assert_eq!(
            id.chars().count(),
            SHA256_HEX_LEN,
            "identifier for {:?} was not {} chars",
            key,
            SHA256_HEX_LEN
        );
        prop_assert!(
            id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "identifier {:?} for key {:?} contained non-lowercase-hex chars",
            id,
            key
        );

        // Non-reversible: the identifier is never equal to the plaintext key.
        // (A 64-char lowercase-hex string can never equal a key that is not
        // itself exactly that string; the assertion guards the contract.)
        prop_assert_ne!(
            &id,
            &key,
            "identifier equalled the plaintext key {:?}",
            key
        );

        // Distinct keys produce distinct identifiers: whenever the two
        // generated keys differ, their identifiers must differ as well.
        if key != other {
            prop_assert_ne!(
                key_identifier(&key),
                key_identifier(&other),
                "distinct keys {:?} and {:?} collided",
                key,
                other
            );
        }
    }

    // Stronger collision check across a batch of distinct keys: the number of
    // unique identifiers must equal the number of unique input keys.
    #[test]
    fn prop_distinct_keys_have_distinct_identifiers(
        keys in prop::collection::vec(any::<String>(), 1..50),
    ) {
        let mut by_id: HashMap<String, String> = HashMap::new();
        for key in &keys {
            let id = key_identifier(key);
            if let Some(prev) = by_id.insert(id.clone(), key.clone()) {
                // A shared identifier is only acceptable if the keys are equal.
                prop_assert_eq!(
                    &prev,
                    key,
                    "distinct keys {:?} and {:?} produced the same identifier {:?}",
                    prev,
                    key,
                    id
                );
            }
        }
    }
}
