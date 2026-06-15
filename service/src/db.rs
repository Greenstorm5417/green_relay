use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Acquire, Row, SqlitePool};

use crate::models::{InboundMessage, MessageStatus, OutboundMessage};

struct Migration {
    version: i64,
    sql: &'static str,
}

const SCHEMA_V1: &str = include_str!("sql/schema_v1.sql");

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    sql: SCHEMA_V1,
}];

pub use crate::error::DbError;

impl From<sqlx::Error> for DbError {
    fn from(e: sqlx::Error) -> Self {
        DbError::Sqlx(e)
    }
}

/// Provides access to the persistent SQLite database pool.
#[derive(Clone)]
pub struct Db {
    pool: SqlitePool,
    schema_ready: Arc<AtomicBool>,
}

/// A raw API-key row for the admin listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiKeyRecord {
    pub id: i64,
    pub key_identifier: String,
    pub custom_rate_limit: Option<u32>,
    pub revoked: bool,
    pub created_at: DateTime<Utc>,
}

/// A recent outbound-message row for the admin activity feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundActivityRow {
    pub created_at: DateTime<Utc>,
    pub status: String,
    pub to_number: String,
}

/// A recent inbound-message row for the admin activity feed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundActivityRow {
    pub received_at: DateTime<Utc>,
    pub from_number: String,
}

impl Db {
    /// Connects to the SQLite database at the specified path.
    pub async fn connect(database_path: &str) -> Result<Db, DbError> {
        let options = SqliteConnectOptions::new()
            .filename(database_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = SqlitePoolOptions::new().connect_with(options).await?;
        Ok(Db {
            pool,
            schema_ready: Arc::new(AtomicBool::new(false)),
        })
    }

    /// Connects to an in-memory SQLite database for testing.
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

    /// Initializes a database connection and runs pending migrations.
    pub async fn initialize(database_path: &str) -> Result<Db, DbError> {
        let db = Db::connect(database_path).await?;
        db.run_migrations().await?;
        Ok(db)
    }

    /// Returns a reference to the underlying SQLite connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Returns true if all pending migrations have run and schema is ready.
    pub fn is_schema_ready(&self) -> bool {
        self.schema_ready.load(Ordering::SeqCst)
    }

    /// Executes all outstanding schema migrations.
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

    fn ensure_ready(&self) -> Result<(), DbError> {
        if self.is_schema_ready() {
            Ok(())
        } else {
            Err(DbError::NotReady)
        }
    }

    /// Inserts a new outbound message record into the database.
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

        let result = sqlx::query(
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
        .execute(self.pool())
        .await?;

        Ok(OutboundMessage {
            id: result.last_insert_rowid(),
            to_number: to_number.to_string(),
            body: body.to_string(),
            status,
            part_count,
            msg_reference: None,
            error_code: None,
            created_at: now,
            updated_at: now,
        })
    }

    /// Updates the status, message reference, or error code of an outbound message.
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

    /// Updates an outbound message's status fields without reading the row back.
    ///
    /// A lighter-weight alternative to [`Db::update_outbound_message`] for the
    /// background dispatch path, which discards the returned record. Runs a
    /// single autocommit `UPDATE` rather than an `UPDATE` plus a `SELECT`
    /// read-back wrapped in a transaction.
    pub async fn set_outbound_status(
        &self,
        id: i64,
        status: MessageStatus,
        msg_reference: Option<&str>,
        error_code: Option<&str>,
    ) -> Result<(), DbError> {
        self.ensure_ready()?;

        let now_text = Utc::now().to_rfc3339();

        let result = sqlx::query(
            "UPDATE outbound_messages \
                SET status = ?, msg_reference = ?, error_code = ?, updated_at = ? \
              WHERE id = ?",
        )
        .bind(status.as_db_str())
        .bind(msg_reference)
        .bind(error_code)
        .bind(&now_text)
        .bind(id)
        .execute(self.pool())
        .await?;

        if result.rows_affected() == 0 {
            return Err(DbError::Sqlx(sqlx::Error::RowNotFound));
        }
        Ok(())
    }

    /// Fetches a single outbound message record by its ID.
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

    /// Looks up an active (non-revoked) API key by its identifier.
    ///
    /// Returns the key's row id and its optional custom rate limit. A stored
    /// custom limit that does not fit in a `u32` is treated as absent so the
    /// caller falls back to the default limit.
    pub async fn lookup_active_key(
        &self,
        identifier: &str,
    ) -> Result<Option<(i64, Option<u32>)>, DbError> {
        let row = sqlx::query(
            "SELECT id, custom_rate_limit FROM api_keys \
               WHERE key_identifier = ? AND revoked = 0",
        )
        .bind(identifier)
        .fetch_optional(self.pool())
        .await?;

        match row {
            Some(row) => {
                let id: i64 = row.try_get("id")?;
                let custom: Option<i64> = row.try_get("custom_rate_limit")?;
                Ok(Some((id, custom.and_then(|c| u32::try_from(c).ok()))))
            }
            None => Ok(None),
        }
    }

    /// Inserts an audit-log record. Centralizes the audit `INSERT` used by the
    /// admin and modem layers.
    pub async fn insert_audit(
        &self,
        event_type: &str,
        key_identifier: Option<&str>,
        detail: Option<&str>,
        at: DateTime<Utc>,
    ) -> Result<(), DbError> {
        sqlx::query(
            "INSERT INTO audit_log (event_type, key_identifier, detail, created_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(event_type)
        .bind(key_identifier)
        .bind(detail)
        .bind(at.to_rfc3339())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Looks up an admin user's id and stored password hash by username.
    pub async fn find_admin_credentials(
        &self,
        username: &str,
    ) -> Result<Option<(i64, String)>, DbError> {
        let row = sqlx::query("SELECT id, password_hash FROM admin_users WHERE username = ?")
            .bind(username)
            .fetch_optional(self.pool())
            .await?;
        match row {
            Some(row) => {
                let id: i64 = row.try_get("id")?;
                let hash: String = row.try_get("password_hash")?;
                Ok(Some((id, hash)))
            }
            None => Ok(None),
        }
    }

    /// Inserts a new (non-revoked) API key, returning its row id.
    pub async fn insert_api_key(&self, identifier: &str, created_at: &str) -> Result<i64, DbError> {
        let result = sqlx::query(
            "INSERT INTO api_keys (key_hash, key_identifier, custom_rate_limit, revoked, created_at) \
             VALUES (?, ?, NULL, 0, ?)",
        )
        .bind(identifier)
        .bind(identifier)
        .bind(created_at)
        .execute(self.pool())
        .await?;
        Ok(result.last_insert_rowid())
    }

    /// Lists all API keys, newest first.
    pub async fn list_api_keys(&self) -> Result<Vec<ApiKeyRecord>, DbError> {
        let rows = sqlx::query(
            "SELECT id, key_identifier, custom_rate_limit, revoked, created_at \
             FROM api_keys ORDER BY created_at DESC, id DESC",
        )
        .fetch_all(self.pool())
        .await?;

        let mut keys = Vec::with_capacity(rows.len());
        for row in &rows {
            let created_at: String = row.try_get("created_at")?;
            let custom: Option<i64> = row.try_get("custom_rate_limit")?;
            let revoked: i64 = row.try_get("revoked")?;
            keys.push(ApiKeyRecord {
                id: row.try_get("id")?,
                key_identifier: row.try_get("key_identifier")?,
                custom_rate_limit: custom.and_then(|v| u32::try_from(v).ok()),
                revoked: revoked != 0,
                created_at: DateTime::parse_from_rfc3339(&created_at)
                    .map(|dt| dt.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now()),
            });
        }
        Ok(keys)
    }

    /// Returns the public identifier of an API key by row id, if it exists.
    pub async fn api_key_identifier(&self, id: i64) -> Result<Option<String>, DbError> {
        let row = sqlx::query("SELECT key_identifier FROM api_keys WHERE id = ?")
            .bind(id)
            .fetch_optional(self.pool())
            .await?;
        match row {
            Some(row) => Ok(Some(row.try_get("key_identifier")?)),
            None => Ok(None),
        }
    }

    /// Marks an API key revoked, returning the number of rows affected.
    pub async fn set_api_key_revoked(&self, id: i64) -> Result<u64, DbError> {
        let result = sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected())
    }

    /// Returns recent outbound messages (since `cutoff`, RFC3339) for the
    /// admin activity feed, newest first, capped at 50.
    pub async fn recent_outbound_activity(
        &self,
        cutoff: &str,
    ) -> Result<Vec<OutboundActivityRow>, DbError> {
        let rows = sqlx::query(
            "SELECT created_at, status, to_number FROM outbound_messages \
             WHERE created_at >= ? ORDER BY created_at DESC LIMIT 50",
        )
        .bind(cutoff)
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::new();
        for row in &rows {
            let created_at: String = row.try_get("created_at")?;
            if let Ok(ts) = DateTime::parse_from_rfc3339(&created_at) {
                out.push(OutboundActivityRow {
                    created_at: ts.with_timezone(&Utc),
                    status: row.try_get("status")?,
                    to_number: row.try_get("to_number")?,
                });
            }
        }
        Ok(out)
    }

    /// Returns recent inbound messages (since `cutoff`, RFC3339) for the admin
    /// activity feed, newest first, capped at 50.
    pub async fn recent_inbound_activity(
        &self,
        cutoff: &str,
    ) -> Result<Vec<InboundActivityRow>, DbError> {
        let rows = sqlx::query(
            "SELECT received_at, from_number FROM inbound_messages \
             WHERE received_at >= ? ORDER BY received_at DESC LIMIT 50",
        )
        .bind(cutoff)
        .fetch_all(self.pool())
        .await?;

        let mut out = Vec::new();
        for row in &rows {
            let received_at: String = row.try_get("received_at")?;
            if let Ok(ts) = DateTime::parse_from_rfc3339(&received_at) {
                out.push(InboundActivityRow {
                    received_at: ts.with_timezone(&Utc),
                    from_number: row.try_get("from_number")?,
                });
            }
        }
        Ok(out)
    }

    /// Inserts a new inbound message record into the database.
    pub async fn create_inbound_message(
        &self,
        from_number: &str,
        body: &str,
        received_at: DateTime<Utc>,
    ) -> Result<InboundMessage, DbError> {
        self.ensure_ready()?;

        let received_text = received_at.to_rfc3339();

        let result = sqlx::query(
            "INSERT INTO inbound_messages (from_number, body, received_at) VALUES (?, ?, ?)",
        )
        .bind(from_number)
        .bind(body)
        .bind(&received_text)
        .execute(self.pool())
        .await?;

        Ok(InboundMessage {
            id: result.last_insert_rowid(),
            from_number: from_number.to_string(),
            body: body.to_string(),
            received_at,
        })
    }

    /// Fetches a single inbound message record by its ID.
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

    /// Lists all inbound messages in descending order of receipt.
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

    /// Create a new admin user, or reset an existing one's password.
    ///
    /// Used to bootstrap the first administrator (the table starts empty) and
    /// to recover access. When the username already exists its password hash is
    /// replaced and any failed-attempt lockout is cleared; otherwise a new row
    /// is inserted. Returns `true` when a new user was created, `false` when an
    /// existing user was updated. The write is transactional.
    pub async fn upsert_admin_user(
        &self,
        username: &str,
        password_hash: &str,
    ) -> Result<bool, DbError> {
        self.ensure_ready()?;

        let now_text = Utc::now().to_rfc3339();
        let mut tx = self.pool.begin().await?;

        let update = sqlx::query(
            "UPDATE admin_users \
                SET password_hash = ?, failed_attempts = 0, locked_until = NULL \
              WHERE username = ?",
        )
        .bind(password_hash)
        .bind(username)
        .execute(&mut *tx)
        .await;

        let updated = match update {
            Ok(result) => result.rows_affected(),
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
        };

        let created = if updated == 0 {
            let insert = sqlx::query(
                "INSERT INTO admin_users \
                     (username, password_hash, failed_attempts, locked_until, created_at) \
                 VALUES (?, ?, 0, NULL, ?)",
            )
            .bind(username)
            .bind(password_hash)
            .bind(&now_text)
            .execute(&mut *tx)
            .await;

            if let Err(e) = insert {
                let _ = tx.rollback().await;
                return Err(DbError::Sqlx(e));
            }
            true
        } else {
            false
        };

        tx.commit().await?;
        Ok(created)
    }
}

/// Warns if the database storage used has reached 90% or more of capacity.
pub fn storage_capacity_warn(used: u32, total: u32) -> bool {
    if total == 0 {
        return false;
    }
    u64::from(used) * 10 >= u64::from(total) * 9
}

fn parse_ts(text: &str) -> Result<DateTime<Utc>, DbError> {
    DateTime::parse_from_rfc3339(text)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| DbError::Sqlx(sqlx::Error::Decode(Box::new(e))))
}

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

    #[tokio::test]
    async fn schema_ready_gate_opens_after_migrations() {
        let db = Db::connect_in_memory().await.unwrap();
        assert!(!db.is_schema_ready());
        db.run_migrations().await.unwrap();
        assert!(db.is_schema_ready());
    }

    #[tokio::test]
    async fn writes_before_ready_are_rejected() {
        let db = Db::connect_in_memory().await.unwrap();
        let err = db
            .create_outbound_message("+14155552671", "hi", MessageStatus::Queued, 1)
            .await
            .unwrap_err();
        assert!(err.is_not_ready());
    }

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

    #[tokio::test]
    async fn list_inbound_empty_returns_empty() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let listed = db.list_inbound_messages().await.unwrap();
        assert!(listed.is_empty());
    }

    #[tokio::test]
    async fn list_inbound_returns_all_newest_first_and_breaks_ties_with_highest_id() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let base = Utc::now();
        let t1 = base;
        let t2 = base + Duration::from_secs(10);
        let t3 = base + Duration::from_secs(20);

        let m1 = db.create_inbound_message("1", "first", t1).await.unwrap();
        let m2 = db.create_inbound_message("2", "second", t2).await.unwrap();
        let m3 = db
            .create_inbound_message("3", "third_tie_a", t3)
            .await
            .unwrap();
        let m4 = db
            .create_inbound_message("4", "third_tie_b", t3)
            .await
            .unwrap();

        let listed = db.list_inbound_messages().await.unwrap();
        assert_eq!(listed.len(), 4);
        assert_eq!(listed[0], m4);
        assert_eq!(listed[1], m3);
        assert_eq!(listed[2], m2);
        assert_eq!(listed[3], m1);
    }

    #[test]
    fn capacity_alert_triggers_at_90_percent_of_limit() {
        assert!(!storage_capacity_warn(0, 0));
        assert!(!storage_capacity_warn(10, 0));
        assert!(!storage_capacity_warn(0, 10));

        assert!(!storage_capacity_warn(89, 100));
        assert!(storage_capacity_warn(90, 100));
        assert!(storage_capacity_warn(95, 100));

        assert!(!storage_capacity_warn(8, 10));
        assert!(storage_capacity_warn(9, 10));

        assert!(storage_capacity_warn(18, 20));
        assert!(!storage_capacity_warn(17, 20));
    }

    /// Bootstrapping a fresh admin inserts the user, and a second upsert resets
    /// the password in place rather than failing on the unique constraint.
    #[tokio::test]
    async fn upsert_admin_creates_then_resets() {
        let db = Db::connect_in_memory().await.unwrap();
        db.run_migrations().await.unwrap();

        let first_hash = crate::admin::hash_password("first-secret");
        let created = db.upsert_admin_user("root", &first_hash).await.unwrap();
        assert!(created, "first upsert should create the user");

        // Exactly one row, with the original password verifying.
        let (id, stored): (i64, String) =
            sqlx::query_as("SELECT id, password_hash FROM admin_users WHERE username = ?")
                .bind("root")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(crate::admin::verify_password("first-secret", &stored));

        let second_hash = crate::admin::hash_password("second-secret");
        let created_again = db.upsert_admin_user("root", &second_hash).await.unwrap();
        assert!(!created_again, "second upsert should update, not create");

        let (id_after, stored_after): (i64, String) =
            sqlx::query_as("SELECT id, password_hash FROM admin_users WHERE username = ?")
                .bind("root")
                .fetch_one(db.pool())
                .await
                .unwrap();
        // Same row, new password.
        assert_eq!(id, id_after);
        assert!(crate::admin::verify_password(
            "second-secret",
            &stored_after
        ));
        assert!(!crate::admin::verify_password(
            "first-secret",
            &stored_after
        ));
    }
}
