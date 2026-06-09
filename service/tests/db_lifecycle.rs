//! Integration tests for the persistence lifecycle (task 4.7).
//!
//! These tests live in their own integration-test crate (separate from
//! `src/db.rs`) per the spec's test-placement note and exercise the *public*
//! persistence API of the `green_relay` library against real SQLite
//! databases. They cover the database lifecycle described in `design.md` §9:
//!
//! - schema creation on a fresh database (Req 6.2) and the smoke check that all
//!   five record tables exist (Req 6.1);
//! - migrations applied in ascending version order, idempotently (Req 6.3);
//! - pre-ready writes rejected as `NotReady`, which the API maps to HTTP 503
//!   (Req 6.5);
//! - write failures rolling back with no partial change, surfaced as a
//!   server-side error the API maps to HTTP 500 (Req 6.6);
//! - a startup migration failure leaving the schema-ready gate closed (Req 6.7).
//!
//! `src/db.rs` already carries `#[cfg(test)]` unit tests for some of this; these
//! integration tests drive the same behaviour through the published surface.
//!
//! A bare `:memory:` pool gives each pooled connection its *own* empty store, so
//! a write and its read-back can land on different connections and miss the
//! migrated schema. Following the `db_property_23.rs` pattern, every test uses a
//! unique temporary file path (or `Db::initialize`, which pins a shared store)
//! so all operations observe the same database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::Utc;
use green_relay::db::{Db, DbError};
use green_relay::models::MessageStatus;

/// Monotonic counter giving every database a unique on-disk filename so
/// repeated or concurrent tests never share state.
static DB_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary SQLite database file that deletes itself (and any `-wal` /
/// `-shm` sidecar files) when dropped. Mirrors the helper in
/// `db_property_23.rs`.
struct TempDbFile {
    path: PathBuf,
}

impl TempDbFile {
    fn new() -> TempDbFile {
        let seq = DB_SEQ.fetch_add(1, Ordering::Relaxed);
        let name = format!("sms_lifecycle_{}_{}.sqlite", std::process::id(), seq);
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

/// The five record tables the schema must contain (Req 6.1).
const REQUIRED_TABLES: [&str; 5] = [
    "outbound_messages",
    "inbound_messages",
    "api_keys",
    "admin_users",
    "audit_log",
];

/// Whether a table of the given name exists in the connected database.
async fn table_exists(db: &Db, table: &str) -> bool {
    let found: Option<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(table)
            .fetch_optional(db.pool())
            .await
            .expect("query sqlite_master");
    found.as_deref() == Some(table)
}

/// Req 6.2: starting against an absent schema creates it before the service
/// becomes ready. The gate is closed on `connect` and opens only after
/// `run_migrations` succeeds, and the migration ledger records version 1.
#[tokio::test]
async fn fresh_database_creates_schema_and_opens_gate() {
    let file = TempDbFile::new();

    let db = Db::connect(file.path_str())
        .await
        .expect("connect to fresh database");

    // Before migrations the schema-ready gate is closed.
    assert!(!db.is_schema_ready(), "gate must start closed");

    db.run_migrations().await.expect("run migrations");

    // After migrations the gate is open and the ledger records the migration.
    assert!(db.is_schema_ready(), "gate must open after migrations");

    let recorded: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
        .fetch_one(db.pool())
        .await
        .expect("count recorded migrations");
    assert!(recorded >= 1, "at least one migration should be recorded");
}

/// Req 6.1: smoke check that the schema contains tables for all five record
/// types after the lifecycle runs.
#[tokio::test]
async fn all_five_record_tables_exist_after_migrations() {
    let file = TempDbFile::new();
    let db = Db::initialize(file.path_str())
        .await
        .expect("initialize database");

    for table in REQUIRED_TABLES {
        assert!(
            table_exists(&db, table).await,
            "expected table `{table}` to exist after migrations"
        );
    }
}

/// Req 6.3: pending migrations are applied in ascending version order, and a
/// second run is idempotent (applies nothing, gate stays open).
#[tokio::test]
async fn migrations_apply_in_ascending_order_and_are_idempotent() {
    let file = TempDbFile::new();
    let db = Db::initialize(file.path_str())
        .await
        .expect("initialize database");

    // The recorded versions are strictly increasing (ascending order).
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM schema_migrations ORDER BY version ASC")
            .fetch_all(db.pool())
            .await
            .expect("read recorded migration versions");
    assert!(!versions.is_empty(), "expected at least one migration");
    for pair in versions.windows(2) {
        assert!(
            pair[0] < pair[1],
            "migration versions must be strictly ascending, saw {pair:?}"
        );
    }

    let count_before = versions.len();

    // Running migrations again must be a no-op: no new versions recorded and
    // the gate remains open.
    db.run_migrations().await.expect("re-run migrations");
    assert!(db.is_schema_ready(), "gate stays open after re-run");

    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
        .fetch_one(db.pool())
        .await
        .expect("count migrations after re-run");
    assert_eq!(
        count_after as usize, count_before,
        "re-running migrations must not record new versions"
    );
}

/// Req 6.5: a message-record change requested while the schema is not ready is
/// rejected without modifying any record, and the error is the `NotReady`
/// condition the API maps to HTTP 503. Covers outbound create, outbound update,
/// and inbound create.
#[tokio::test]
async fn writes_before_ready_are_rejected_as_not_ready() {
    let file = TempDbFile::new();

    // Connected but migrations never run: the gate is closed.
    let db = Db::connect(file.path_str())
        .await
        .expect("connect to fresh database");
    assert!(!db.is_schema_ready());

    let create_err = db
        .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
        .await
        .expect_err("create must be rejected before ready");
    assert!(
        create_err.is_not_ready(),
        "outbound create before ready must be NotReady (HTTP 503)"
    );

    let update_err = db
        .update_outbound_message(1, MessageStatus::Sent, None, None)
        .await
        .expect_err("update must be rejected before ready");
    assert!(
        update_err.is_not_ready(),
        "outbound update before ready must be NotReady (HTTP 503)"
    );

    let inbound_err = db
        .create_inbound_message("+14155550000", "incoming", Utc::now())
        .await
        .expect_err("inbound create must be rejected before ready");
    assert!(
        inbound_err.is_not_ready(),
        "inbound create before ready must be NotReady (HTTP 503)"
    );
}

/// Req 6.6: a failing write rolls back so no partial change is persisted, and
/// the failure is a server-side error (not `NotReady`) the API maps to HTTP
/// 500. Updating a non-existent row exercises the rollback branch.
#[tokio::test]
async fn write_failure_rolls_back_and_is_server_error() {
    let file = TempDbFile::new();
    let db = Db::initialize(file.path_str())
        .await
        .expect("initialize database");

    // One legitimately persisted record establishes the baseline row count.
    db.create_outbound_message("+14155552671", "hello", MessageStatus::Queued, 1)
        .await
        .expect("create baseline outbound message");

    let count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbound_messages")
        .fetch_one(db.pool())
        .await
        .expect("count outbound before failed write");

    // Updating a row that does not exist fails after the schema is ready.
    let err = db
        .update_outbound_message(9_999, MessageStatus::Sent, Some("42"), None)
        .await
        .expect_err("update of missing row must fail");

    // The failure maps to HTTP 500, not the 503 NotReady condition.
    assert!(
        !err.is_not_ready(),
        "a write failure must be a server error (HTTP 500), not NotReady"
    );

    // Rollback: the failed write left no partial change behind.
    let count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbound_messages")
        .fetch_one(db.pool())
        .await
        .expect("count outbound after failed write");
    assert_eq!(
        count_after, count_before,
        "failed write must not change the persisted row count"
    );
}

/// Req 6.7: if a migration fails during startup, the error is surfaced and the
/// schema-ready gate stays closed so the service refuses to serve.
///
/// The failure is forced by pre-creating the migration ledger with an extra
/// `NOT NULL` column that has no default, so the migration's bookkeeping
/// `INSERT INTO schema_migrations (version, applied_at)` violates the
/// constraint. `CREATE TABLE IF NOT EXISTS schema_migrations` then skips the
/// existing table, and `SELECT MAX(version)` still works, so the loop enters
/// and fails on the insert.
#[tokio::test]
async fn startup_migration_failure_keeps_gate_closed() {
    let file = TempDbFile::new();

    let db = Db::connect(file.path_str())
        .await
        .expect("connect to fresh database");

    sqlx::raw_sql(
        "CREATE TABLE schema_migrations (\
             version INTEGER PRIMARY KEY, \
             applied_at TEXT NOT NULL, \
             extra TEXT NOT NULL\
         )",
    )
    .execute(db.pool())
    .await
    .expect("seed a conflicting migration ledger");

    let err = db
        .run_migrations()
        .await
        .expect_err("migration must fail against the conflicting ledger");

    assert!(
        matches!(err, DbError::Migration { version: 1, .. }),
        "expected a migration failure for version 1, got {err:?}"
    );

    // The gate stays closed so the service will refuse to serve.
    assert!(
        !db.is_schema_ready(),
        "schema-ready gate must stay closed after a migration failure"
    );

    // And writes are still rejected as NotReady (HTTP 503) afterwards.
    let write_err = db
        .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
        .await
        .expect_err("writes must remain rejected after migration failure");
    assert!(write_err.is_not_ready());
}
