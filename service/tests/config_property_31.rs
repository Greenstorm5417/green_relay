//! Property-based test for configuration merge (Property 31).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/config.rs`) per the spec's test-placement note, and exercises the
//! public `merge_env_over_file` function of the `green_relay` library.

use std::collections::HashMap;

use proptest::prelude::*;
use green_relay::config::merge_env_over_file;

/// Generate a small key from a constrained alphabet so that the file and
/// environment maps share keys frequently enough to exercise the override
/// path (env wins) as well as the file-only and env-only paths.
fn any_key() -> impl Strategy<Value = String> {
    proptest::sample::select(vec!["A", "B", "C", "D", "E", "F", "G", "H"])
        .prop_map(|s| s.to_string())
}

/// Generate an arbitrary string value.
fn any_value() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_./:+-]{0,12}"
}

/// Generate a key/value map with up to 8 distinct keys.
fn any_map() -> impl Strategy<Value = HashMap<String, String>> {
    proptest::collection::hash_map(any_key(), any_value(), 0..=8)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 31: Configuration merge prefers
    // environment. For any file-sourced configuration map and
    // environment-sourced map, the merged value for a key equals the
    // environment value when the key is defined in the environment, and
    // otherwise equals the file value.
    //
    // Validates: Requirements 11.1
    #[test]
    fn prop_configuration_merge_prefers_environment(
        file in any_map(),
        env in any_map(),
    ) {
        let merged = merge_env_over_file(&file, &env);

        // 1. The merged map contains exactly the union of keys from both maps:
        //    every file key and every env key appears, and no extra keys exist.
        for key in file.keys() {
            prop_assert!(
                merged.contains_key(key),
                "merged map must retain file key {:?}",
                key
            );
        }
        for key in env.keys() {
            prop_assert!(
                merged.contains_key(key),
                "merged map must contain env key {:?}",
                key
            );
        }
        for key in merged.keys() {
            prop_assert!(
                file.contains_key(key) || env.contains_key(key),
                "merged map must not invent key {:?}",
                key
            );
        }

        // 2. For every merged key, the value equals the env value when the key
        //    is defined in env, otherwise it equals the file value.
        for (key, merged_value) in &merged {
            match env.get(key) {
                Some(env_value) => prop_assert_eq!(
                    merged_value,
                    env_value,
                    "key {:?} present in env must take the env value",
                    key
                ),
                None => {
                    let file_value = file
                        .get(key)
                        .expect("a merged key absent from env must come from file");
                    prop_assert_eq!(
                        merged_value,
                        file_value,
                        "key {:?} absent from env must take the file value",
                        key
                    );
                }
            }
        }
    }
}
