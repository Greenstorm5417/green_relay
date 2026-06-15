//! API-key administration: the [`ApiKeyView`] model and the create/list/revoke
//! domain operations. Persistence lives in [`crate::db`]; this layer maps rows
//! to views and records audit events.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::auth::key_identifier;
use crate::db::DbError;

use super::AdminState;
use super::session::random_token;

/// View model representing an API key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiKeyView {
    /// The unique identifier.
    pub id: i64,
    /// The key's public identifier.
    pub key_identifier: String,
    /// Custom rate limit override.
    pub custom_rate_limit: Option<u32>,
    /// Revocation flag.
    pub revoked: bool,
    /// Key creation timestamp.
    pub created_at: DateTime<Utc>,
}

/// Creates a new API key and returns its plaintext value.
pub async fn create_api_key(
    state: &AdminState,
    now_utc: DateTime<Utc>,
) -> Result<(String, ApiKeyView), DbError> {
    let plaintext = format!("sk_{}", random_token());
    let identifier = key_identifier(&plaintext);

    let id = state
        .db
        .insert_api_key(&identifier, &now_utc.to_rfc3339())
        .await?;

    state
        .db
        .insert_audit(
            "api_key_created",
            Some(&identifier),
            Some("admin created a new API key"),
            now_utc,
        )
        .await?;

    Ok((
        plaintext,
        ApiKeyView {
            id,
            key_identifier: identifier,
            custom_rate_limit: None,
            revoked: false,
            created_at: now_utc,
        },
    ))
}

/// Lists all registered API keys.
pub async fn list_api_keys(state: &AdminState) -> Result<Vec<ApiKeyView>, DbError> {
    let records = state.db.list_api_keys().await?;
    Ok(records
        .into_iter()
        .map(|record| ApiKeyView {
            id: record.id,
            key_identifier: record.key_identifier,
            custom_rate_limit: record.custom_rate_limit,
            revoked: record.revoked,
            created_at: record.created_at,
        })
        .collect())
}

/// Revokes an API key, returning the number of rows affected.
pub async fn revoke_api_key(
    state: &AdminState,
    id: i64,
    now_utc: DateTime<Utc>,
) -> Result<u64, DbError> {
    let identifier = state.db.api_key_identifier(id).await?;
    let affected = state.db.set_api_key_revoked(id).await?;

    if affected > 0 {
        state
            .db
            .insert_audit(
                "api_key_revoked",
                identifier.as_deref(),
                Some("admin revoked an API key"),
                now_utc,
            )
            .await?;
    }
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::testutil::{audit_count, test_state};

    #[tokio::test]
    async fn create_list_and_revoke_keys() {
        let state = test_state().await;
        let (plaintext, view) = create_api_key(&state, Utc::now()).await.unwrap();
        assert!(plaintext.starts_with("sk_"));
        assert_ne!(view.key_identifier, plaintext);
        let _ = create_api_key(&state, Utc::now()).await.unwrap();
        let keys = list_api_keys(&state).await.unwrap();
        assert_eq!(keys.len(), 2);
        assert!(keys.iter().all(|k| !k.revoked));
        let target = keys[0].id;
        let affected = revoke_api_key(&state, target, Utc::now()).await.unwrap();
        assert_eq!(affected, 1);
        let keys = list_api_keys(&state).await.unwrap();
        let revoked = keys.iter().find(|k| k.id == target).unwrap();
        assert!(revoked.revoked);
        assert_eq!(audit_count(&state, "api_key_created").await, 2);
        assert_eq!(audit_count(&state, "api_key_revoked").await, 1);
    }

    #[tokio::test]
    async fn revoke_unknown_key_affects_no_rows() {
        let state = test_state().await;
        let affected = revoke_api_key(&state, 9999, Utc::now()).await.unwrap();
        assert_eq!(affected, 0);
    }
}
