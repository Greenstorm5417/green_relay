//! SQLite persistence layer: pool, schema, migrations, and queries.
//! Migrations are applied on startup before the service accepts requests.
//! Writes are transactional with rollback on failure.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Acquire, Row, SqlitePool};

use crate::models::{InboundMessage, MessageStatus, OutboundMessage};

/// Ordered, versioned schema migration.
struct Migration {
    version: i64,
    sql: &'static str,
}

/// Migration v1: create full schema for all five record types.
const SCHEMA_V1: &str = r#"
CREATE TABLE IF NOT EXISTS outbound_messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    to_number     TEXT    NOT NULL,
    body          TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    part_count    INTEGER NOT NULL,
    msg_reference TEXT,
    error_code    TEXT,
    created_at    TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS inbound_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    from_number TEXT    NOT NULL,
    body        TEXT    NOT NULL,
    received_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    key_hash          TEXT    NOT NULL UNIQUE,
    key_identifier    TEXT    NOT NULL UNIQUE,
    custom_rate_limit INTEGER,
    revoked           INTEGER NOT NULL DEFAULT 0,
    created_at        TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS admin_users (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    username        TEXT    NOT NULL UNIQUE,
    password_hash   TEXT    NOT NULL,
    failed_attempts INTEGER NOT NULL DEFAULT 0,
    locked_until    TEXT,
    created_at      TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS audit_log (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type     TEXT    NOT NULL,
    key_identifier TEXT,
    detail         TEXT,
    created_at     TEXT    NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_inbound_received_at
    ON inbound_messages (received_at DESC);
"#;

/// All migrations in ascending version order.
const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: SCHEMA_V1,
}];

/// Errors produced by the persistence layer.
#[derive(Debug)]
pub enum DbError {
    /// Schema gate not yet open.
    NotReady,
    /// Schema migration failed.
    Migration { version: i64, source: sqlx::Error },
    /// Database query or connection error.
    Sqlx(sqlx::Error),
}

impl DbError {
    /// Whether this is the "not ready" condition (HTTP 503).
    pub fn is_not_ready(&self) -> bool {
        matches!(self, DbError::NotReady)
    }
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::NotReady => write!(f, "database schema is not ready"),
            DbError::Migration { version, source } => {
                write!(f, "migration {version} failed: {source}")
            }
            DbError::Sqlx(e) => write!(f, "database error: {e}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Migration { source, .. } => Some(source),
            DbError::Sqlx(e) => Some(e),
            DbError::NotReady => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        DbError::Sqlx(e)
    }
}

/// Persistence handle shared across the service.
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    schema_ready: Arc<AtomicBool>,
}

impl Db {
    /// Connect to SQLite database, creating if missing.
    pub async fn connect(database_path: &str) -> Result<Db, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Full)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        Ok(Db {
            pool,
            schema_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Connect to private in-memory database for tests.
    #[cfg(test)]
    pub async fn connect_in_memory() -> Result<Db, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .min_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        Ok(Db {
            pool,
            schema_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Connect and run migrations in one step.
    pub async fn initialize(database_path: &str) -> Result<Db, DbError> {
        let db = Db::connect(database_path).await?;
        db.run_migrations().await?;
        Ok(db)
    }

    /// Borrow the underlying connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Whether the schema-ready gate is open.
    pub fn is_schema_ready(&self) -> bool {
        self.schema_ready.load(Ordering::SeqCst)
    }

    /// Create schema if absent, otherwise apply pending migrations.
    pub async fn run_migrations(&self) -> Result<(), DbError> {
        let mut conn = self.pool.acquire().await?;

        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS schema_migrations (\
                 version INTEGER PRIMARY KEY, \
                 applied_at TEXT NOT NULL\
             )",
        )
        .execute(&mut *conn)
        .await?;

        let current: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM schema_migrations")
            .fetch_one(&mut *conn)
            .await?;
        let current = current.unwrap_or(0);

        for migration in MIGRATIONS {
            if migration.version <= current {
                continue;
            }

            let mut tx = conn.begin().await.map_err(|e| DbError::Migration {
                version: migration.version,
                source: e,
            })?;

            sqlx::raw_sql(migration.sql)
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::Migration {
                    version: migration.version,
                    source: e,
                })?;

            sqlx::query("INSERT INTO schema_migrations (version, applied_at) VALUES (?, ?)")
                .bind(migration.version)
                .bind(Utc::now().to_rfc3339())
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::Migration {
                    version: migration.version,
                    source: e,
                })?;

            tx.commit().await.map_err(|e| DbError::Migration {
                version: migration.version,
                source: e,
            })?;
        }

        self.schema_ready.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Schema-ready gate guard for message-record writes.
    fn ensure_ready(&self) -> Result<(), DbError> {
        if self.is_schema_ready() {
            Ok(())
        } else {
            Err(DbError::NotReady)
        }
    }

    /// Create a new outbound message record transactionally.
    pub async fn create_outbound_message(
        &self,
        to_number: &str,
        body: &str,
        status: MessageStatus,
        part_count: u8,
    ) -> Result<OutboundMessage, DbError> {
        self.ensure_ready()?;

        let now = Utc::now();
        let now_text = now.to_rfc3339();

        let mut tx = self.pool.begin().await?;

        let insert = sqlx::query(
            "INSERT INTO outbound_messages \
                 (to_number, body, status, part_count, msg_reference, error_code, created_at, updated_at) \
             VALUES (?, ?, ?, ?, NULL, NULL, ?, ?)",
        )
        .bind(to_number)
        .bind(body)
        .bind(status.as_db_str())
        .bind(part_count as i64)
        .bind(&now_text)
        .bind(&now_text)
        .execute(&mut *tx)
        .await;

        let id = match insert {
            Ok(result) => result.last_insert_rowid(),
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
        };

        tx.commit().await?;

        Ok(OutboundMessage {
            id,
            to_number: to_number.to_string(),
            body: body.to_string(),
            status,
            part_count,
            msg_reference: None,
            error_code: None,
            created_at: parse_ts(&now_text)?,
            updated_at: parse_ts(&now_text)?,
        })
    }

    /// Update outbound message status, reference, and error code transactionally.
    pub async fn update_outbound_message(
        &self,
        id: i64,
        status: MessageStatus,
        msg_reference: Option<&str>,
        error_code: Option<&str>,
    ) -> Result<OutboundMessage, DbError> {
        self.ensure_ready()?;

        let now_text = Utc::now().to_rfc3339();

        let mut tx = self.pool.begin().await?;

        let update = sqlx::query(
            "UPDATE outbound_messages \
                SET status = ?, msg_reference = ?, error_code = ?, updated_at = ? \
              WHERE id = ?",
        )
        .bind(status.as_db_str())
        .bind(msg_reference)
        .bind(error_code)
        .bind(&now_text)
        .bind(id)
        .execute(&mut *tx)
        .await;

        match update {
            Ok(result) if result.rows_affected() == 0 => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(sqlx::Error::RowNotFound));
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
        }

        let row = match sqlx::query(
            "SELECT id, to_number, body, status, part_count, msg_reference, error_code, created_at, updated_at \
               FROM outbound_messages WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        {
            Ok(row) => row,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
        };

        let message = outbound_from_row(&row)?;
        tx.commit().await?;
        Ok(message)
    }

    /// Fetch a single outbound message by id.
    pub async fn get_outbound_message(&self, id: i64) -> Result<Option<OutboundMessage>, DbError> {
        let row = sqlx::query(
            "SELECT id, to_number, body, status, part_count, msg_reference, error_code, created_at, updated_at \
               FROM outbound_messages WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => Ok(Some(outbound_from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// Create a new inbound message record transactionally.
    pub async fn create_inbound_message(
        &self,
        from_number: &str,
        body: &str,
        received_at: DateTime<Utc>,
    ) -> Result<InboundMessage, DbError> {
        self.ensure_ready()?;

        let received_text = received_at.to_rfc3339();

        let mut tx = self.pool.begin().await?;

        let insert = sqlx::query(
            "INSERT INTO inbound_messages (from_number, body, received_at) VALUES (?, ?, ?)",
        )
        .bind(from_number)
        .bind(body)
        .bind(&received_text)
        .execute(&mut *tx)
        .await;

        let id = match insert {
            Ok(result) => result.last_insert_rowid(),
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
        };

        tx.commit().await?;

        Ok(InboundMessage {
            id,
            from_number: from_number.to_string(),
            body: body.to_string(),
            received_at: parse_ts(&received_text)?,
        })
    }

    /// Fetch a single inbound message by id.
    pub async fn get_inbound_message(&self, id: i64) -> Result<Option<InboundMessage>, DbError> {
        let row = sqlx::query(
            "SELECT id, from_number, body, received_at FROM inbound_messages WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => Ok(Some(inbound_from_row(&row)?)),
            None => Ok(None),
        }
    }

    /// List all inbound messages ordered by receipt timestamp descending.
    pub async fn list_inbound_messages(&self) -> Result<Vec<InboundMessage>, DbError> {
        let rows = sqlx::query(
            "SELECT id, from_number, body, received_at \
               FROM inbound_messages \
              ORDER BY received_at DESC, id DESC",
        )
        .fetch_all(self.pool())
        .await?;

        rows.iter().map(inbound_from_row).collect()
    }
}

/// Storage capacity warning decision: true if used >= 90% of total.
pub fn storage_capacity_warn(used: u32, total: u32) -> bool {
    if total == 0 {
        return false;
    }
    u64::from(used) * 10 >= u64::from(total) * 9
}

/// Parse RFC 3339 timestamp back into UTC DateTime.
fn parse_ts(text: &str) -> Result<DateTime<Utc>, DbError> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| DbError::Sqlx(sqlx::Error::Decode(Box::new(e))))
}

/// Map a row from outbound_messages into OutboundMessage.
fn outbound_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<OutboundMessage, DbError> {
    let status_text: String = row.try_get("status")?;
    let status = MessageStatus::from_db_str(&status_text).ok_or_else(|| {
        DbError::Sqlx(sqlx::Error::Decode(
            format!("unknown message status `{status_text}`").into(),
        ))
    })?;
    let part_count: i64 = row.try_get("part_count")?;
    let created_at: String = row.try_get("created_at")?;
    let updated_at: String = row.try_get("updated_at")?;

    Ok(OutboundMessage {
        id: row.try_get("id")?,
        to_number: row.try_get("to_number")?,
        body: row.try_get("body")?,
        status,
        part_count: part_count as u8,
        msg_reference: row.try_get("msg_reference")?,
        error_code: row.try_get("error_code")?,
        created_at: parse_ts(&created_at)?,
        updated_at: parse_ts(&updated_at)?,
    })
}

/// Map a row from inbound_messages into InboundMessage.
fn inbound_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<InboundMessage, DbError> {
    let received_at: String = row.try_get("received_at")?;
    Ok(InboundMessage {
        id: row.try_get("id")?,
        from_number: row.try_get("from_number")?,
        body: row.try_get("body")?,
        received_at: parse_ts(&received_at)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All five tables exist after migrations.
    #[tokio::test]
    async fn migrations_create_all_five_tables() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        for table in [
            "outbound_messages",
            "inbound_messages",
            "api_keys",
            "admin_users",
            "audit_log",
        ] {
            let found: Option<String> = sqlx::query_scalar(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?",
            )
            .bind(table)
            .fetch_optional(db.pool())
            .await
            .unwrap();
            assert_eq!(found.as_deref(), Some(table), "missing table {table}");
        }
    }

    /// Schema-ready gate opens after migrations.
    #[tokio::test]
    async fn schema_ready_gate_opens_after_migrations() {
        let db = Db::connect_in_memory().await.unwrap();
        assert!(!db.is_schema_ready());
        db.run_migrations().await.unwrap();
        assert!(db.is_schema_ready());
    }

    /// Writes before gate opens are rejected.
    #[tokio::test]
    async fn writes_before_ready_are_rejected() {
        let db = Db::connect_in_memory().await.unwrap();
        let err = db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap_err();
        assert!(err.is_not_ready());
    }

    /// Created outbound record can be read back equal.
    #[tokio::test]
    async fn outbound_create_and_read_back() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let created = db
            .create_outbound_message("+14155552671", "hello", MessageStatus::Queued, 1)
            .await
            .unwrap();
        assert_eq!(created.status, MessageStatus::Queued);
        assert_eq!(created.part_count, 1);

        let fetched = db.get_outbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(created, fetched);
    }

    /// Update outbound persists status and reference.
    #[tokio::test]
    async fn outbound_update_persists_status_and_reference() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let created = db
            .create_outbound_message("+14155552671", "hello", MessageStatus::Queued, 1)
            .await
            .unwrap();

        let updated = db
            .update_outbound_message(created.id, MessageStatus::Sent, Some("42"), None)
            .await
            .unwrap();
        assert_eq!(updated.status, MessageStatus::Sent);
        assert_eq!(updated.msg_reference.as_deref(), Some("42"));

        let fetched = db.get_outbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(fetched, updated);
    }

    /// Updating a non-existent row fails (and rolls back) rather than silently
    /// succeeding (Req 6.6).
    #[tokio::test]
    async fn update_missing_outbound_errors() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let err = db
            .update_outbound_message(9999, MessageStatus::Sent, None, None)
            .await
            .unwrap_err();
        assert!(!err.is_not_ready());
    }

    /// An inbound record round-trips through create + read-back (Req 6.4).
    #[tokio::test]
    async fn inbound_create_and_read_back() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let received = Utc::now();
        let created = db
            .create_inbound_message("+14155550000", "incoming", received)
            .await
            .unwrap();
        let fetched = db.get_inbound_message(created.id).await.unwrap().unwrap();
        assert_eq!(created, fetched);
    }

    /// Listing inbound messages on an empty table returns an empty collection
    /// (Req 2.4).
    #[tokio::test]
    async fn list_inbound_empty_returns_empty() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let listed = db.list_inbound_messages().await.unwrap();
        assert!(listed.is_empty());
    }

    /// Listing inbound messages returns every record exactly once, ordered by
    /// receipt timestamp descending regardless of insertion order (Req 2.4).
    #[tokio::test]
    async fn list_inbound_orders_by_received_at_desc() {
        use chrono::TimeZone;

        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let t = |secs: u32| Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, secs).unwrap();

        db.create_inbound_message("+1000", "middle", t(20))
            .await
            .unwrap();
        db.create_inbound_message("+1001", "oldest", t(10))
            .await
            .unwrap();
        db.create_inbound_message("+1002", "newest", t(30))
            .await
            .unwrap();

        let listed = db.list_inbound_messages().await.unwrap();
        let bodies: Vec<&str> = listed.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies, ["newest", "middle", "oldest"]);
        assert_eq!(listed.len(), 3);
    }

    /// The storage-capacity warn decision triggers exactly at the 90% threshold
    /// and treats a zero-capacity report as "no warning" (Req 2.6).
    #[test]
    fn storage_capacity_warn_threshold() {
        // Below 90%.
        assert!(!storage_capacity_warn(89, 100));
        // Exactly 90%.
        assert!(storage_capacity_warn(90, 100));
        // Above 90%.
        assert!(storage_capacity_warn(100, 100));
        // Zero used.
        assert!(!storage_capacity_warn(0, 100));
        // Zero total -> no capacity, no warning.
        assert!(!storage_capacity_warn(0, 0));
        // Integer-exact threshold where floating point could misround:
        // 9/10 = 90% exactly -> warn.
        assert!(storage_capacity_warn(9, 10));
        // 8/10 = 80% -> no warn.
        assert!(!storage_capacity_warn(8, 10));
    }

    /// Running migrations twice is idempotent: the second run applies nothing
    /// and the gate stays open (Req 6.3).
    #[tokio::test]
    async fn migrations_are_idempotent() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();
        db.run_migrations().await.unwrap();
        assert!(db.is_schema_ready());

        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(applied, MIGRATIONS.len() as i64);
    }
}
