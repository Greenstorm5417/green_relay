//! Property-based test for message persistence round-trip (Property 23).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/db.rs`) per the spec's test-placement note, and exercises the public
//! persistence API of the `green_relay` library against an in-memory
//! SQLite database.
//!
//! Validates: Requirements 6.4

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use green_relay::db::Db;
use green_relay::models::MessageStatus;

/// Monotonic counter giving every database a unique on-disk filename so
/// concurrent or repeated cases never share state.
static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary SQLite database file that deletes itself (and any `-wal` /
/// `-shm` sidecar files) when dropped.
///
/// `Db::connect` fixes the pool configuration, and a bare `:memory:` database
/// gives each pooled connection its *own* empty store — so a write and its
/// read-back can land on different connections and miss the migrated schema.
/// A single shared file path avoids that while keeping every case isolated and
/// leaving no residue on disk.
struct TempDbFile {
    path: PathBuf,
}

impl TempDbFile {
    fn new() -> TempDbFile {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("sms_p23_{}_{}.sqlite", std::process::id(), seq);
        TempDbFile {
            path: std::env::temp_dir().join(name),
        }
    }

    fn path_str(&self) -> &str {
        self.path.to_str().expect("temp path is valid UTF-8")
    }
}

impl Drop for TempDbFile {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

/// Build a fresh, migrated database backed by a private temporary file, so a
/// write and its subsequent read-back observe the same store.
async fn fresh_db(file: &TempDbFile) -> Db {
    Db::initialize(file.path_str())
        .await
        .expect("initialize temporary database")
}

/// Generate text that SQLite stores losslessly as UTF-8. NUL bytes are
/// excluded because C-string-based text binding can truncate at an embedded
/// NUL, which is a driver encoding concern rather than a persistence-layer
/// property.
fn arb_text() -> impl Strategy<Value = String> {
    any::<String>().prop_map(|s| s.chars().filter(|c| *c != '\0').collect::<String>())
}

/// Generate one of the three message statuses.
fn arb_status() -> impl Strategy<Value = MessageStatus> {
    prop_oneof![
        Just(MessageStatus::Queued),
        Just(MessageStatus::Sent),
        Just(MessageStatus::Failed),
    ]
}

/// Generate a UTC timestamp in a broad but valid range (epoch .. ~year 2100).
fn arb_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    (0i64..=4_102_444_800i64, 0u32..1_000_000_000u32).prop_map(|(secs, nanos)| {
        Utc.timestamp_opt(secs, nanos)
            .single()
            .unwrap_or_else(|| Utc.timestamp_opt(secs, 0).single().unwrap())
    })
}

/// Run an async body to completion on a fresh single-threaded runtime. proptest
/// test bodies are synchronous, so each case drives the async persistence calls
/// through its own runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
        .block_on(fut)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 23: Message persistence round-trip.
    // For any message record, after it is created or updated through the
    // persistence layer, reading it back returns a record equal to what was
    // written.
    //
    // Validates: Requirements 6.4
    #[test]
    fn prop_message_persistence_round_trip(
        to_number in arb_text(),
        body in arb_text(),
        create_status in arb_status(),
        part_count in 1u8..=10,
        update_status in arb_status(),
        msg_reference in proptest::option::of(arb_text()),
        error_code in proptest::option::of(arb_text()),
        from_number in arb_text(),
        inbound_body in arb_text(),
        received_at in arb_datetime(),
    ) {
        block_on(async {
            let file = TempDbFile::new();
            let db = fresh_db(&file).await;

            // --- Outbound: create then read back ---
            let created = db
                .create_outbound_message(&to_number, &body, create_status, part_count)
                .await
                .expect("create outbound message");
            let fetched_created = db
                .get_outbound_message(created.id)
                .await
                .expect("read outbound message")
                .expect("outbound message exists");
            prop_assert_eq!(
                &created,
                &fetched_created,
                "outbound create round-trip mismatch"
            );

            // --- Outbound: update then read back ---
            let updated = db
                .update_outbound_message(
                    created.id,
                    update_status,
                    msg_reference.as_deref(),
                    error_code.as_deref(),
                )
                .await
                .expect("update outbound message");
            let fetched_updated = db
                .get_outbound_message(created.id)
                .await
                .expect("read updated outbound message")
                .expect("updated outbound message exists");
            prop_assert_eq!(
                &updated,
                &fetched_updated,
                "outbound update round-trip mismatch"
            );

            // --- Inbound: create then read back ---
            let inbound = db
                .create_inbound_message(&from_number, &inbound_body, received_at)
                .await
                .expect("create inbound message");
            let fetched_inbound = db
                .get_inbound_message(inbound.id)
                .await
                .expect("read inbound message")
                .expect("inbound message exists");
            prop_assert_eq!(
                &inbound,
                &fetched_inbound,
                "inbound create round-trip mismatch"
            );

            Ok(())
        })?;
    }
}
