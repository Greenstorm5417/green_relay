//! Property-based test for inbound listing (Property 9).
//!
//! This test lives in its own integration-test crate (separate from
//! `src/db.rs`) per the spec's test-placement note, and exercises the public
//! persistence API of the `green_relay` library against a temporary
//! file-backed SQLite database.
//!
//! Validates: Requirements 2.4

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use proptest::prelude::*;
use green_relay::db::Db;
use green_relay::models::InboundMessage;

/// Monotonic counter giving every database a unique on-disk filename so
/// concurrent or repeated cases never share state.
static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary SQLite database file that deletes itself (and any `-wal` /
/// `-shm` sidecar files) when dropped.
///
/// A bare `:memory:` pool gives each pooled connection its *own* empty store,
/// so a write and a later read can land on different connections and miss the
/// migrated schema. A single shared file path avoids that while keeping every
/// case isolated and leaving no residue on disk.
struct TempDbFile {
    path: PathBuf,
}

impl TempDbFile {
    fn new() -> TempDbFile {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("sms_p09_{}_{}.sqlite", std::process::id(), seq);
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

/// Build a fresh, migrated database backed by a private temporary file.
async fn fresh_db(file: &TempDbFile) -> Db {
    Db::initialize(file.path_str())
        .await
        .expect("initialize temporary database")
}

/// Generate text that SQLite stores losslessly as UTF-8. NUL bytes are
/// excluded because C-string-based text binding can truncate at an embedded
/// NUL, which is a driver encoding concern rather than a listing property.
fn arb_text() -> impl Strategy<Value = String> {
    any::<String>().prop_map(|s| s.chars().filter(|c| *c != '\0').collect::<String>())
}

/// Generate a UTC timestamp in a broad but valid range (epoch .. ~year 2100).
/// Whole-second precision is used so that round-tripping through RFC 3339 text
/// is exact and so that intentional ties on `received_at` occur often enough to
/// exercise the deterministic tie-break.
fn arb_datetime() -> impl Strategy<Value = DateTime<Utc>> {
    (0i64..=4_102_444_800i64).prop_map(|secs| Utc.timestamp_opt(secs, 0).single().unwrap())
}

/// One inbound message to persist: a sender, a body, and a receipt time.
fn arb_inbound() -> impl Strategy<Value = (String, String, DateTime<Utc>)> {
    (arb_text(), arb_text(), arb_datetime())
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

/// A total comparison key for an inbound record that ignores ordering: sort by
/// id so two collections can be compared as multisets.
fn sort_by_id(records: &mut [InboundMessage]) {
    records.sort_by_key(|m| m.id);
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    // Feature: sms-microservice, Property 9: Inbound listing is ordered and lossless.
    // For any set of persisted inbound records, the listing function returns
    // exactly the same multiset of records ordered by receipt timestamp
    // descending, and returns an empty collection for empty input.
    //
    // Validates: Requirements 2.4
    #[test]
    fn prop_inbound_listing_ordered_and_lossless(
        inputs in proptest::collection::vec(arb_inbound(), 0..=25),
    ) {
        block_on(async {
            let file = TempDbFile::new();
            let db = fresh_db(&file).await;

            // Empty input yields an empty collection.
            let empty = db.list_inbound_messages().await.expect("list empty");
            prop_assert!(empty.is_empty(), "empty table must list as empty");

            // Persist every generated record, retaining exactly what was
            // written so we can verify losslessness.
            let mut created: Vec<InboundMessage> = Vec::with_capacity(inputs.len());
            for (from_number, body, received_at) in &inputs {
                let rec = db
                    .create_inbound_message(from_number, body, *received_at)
                    .await
                    .expect("create inbound message");
                created.push(rec);
            }

            let listed = db.list_inbound_messages().await.expect("list inbound");

            // 1. Lossless: the listing returns exactly the persisted multiset,
            //    every record exactly once. Comparing id-sorted vectors proves
            //    no record is dropped, duplicated, or altered.
            prop_assert_eq!(
                listed.len(),
                created.len(),
                "listing must return every record exactly once"
            );
            let mut listed_by_id = listed.clone();
            let mut created_by_id = created.clone();
            sort_by_id(&mut listed_by_id);
            sort_by_id(&mut created_by_id);
            prop_assert_eq!(
                &listed_by_id,
                &created_by_id,
                "listing must be a lossless reordering of the persisted records"
            );

            // 2. Ordered: receipt timestamps are non-increasing (descending),
            //    with ties broken by id descending so the order is total and
            //    deterministic.
            for pair in listed.windows(2) {
                let a = &pair[0];
                let b = &pair[1];
                let ordered = a.received_at > b.received_at
                    || (a.received_at == b.received_at && a.id > b.id);
                prop_assert!(
                    ordered,
                    "records must be ordered by received_at desc (ties by id desc): \
                     ({}, {}) then ({}, {})",
                    a.received_at, a.id, b.received_at, b.id
                );
            }

            Ok(())
        })?;
    }
}
