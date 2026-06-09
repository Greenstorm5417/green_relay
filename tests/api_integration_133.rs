//! HTTP integration tests for the REST API layer (task 13.3).
//!
//! These tests drive the fully-assembled Axum router built by
//! [`sms_micro_service::api::router`] through `tower`'s
//! [`ServiceExt::oneshot`], exercising the public HTTP surface end-to-end
//! without binding a TCP socket or touching real modem hardware. They cover:
//!
//! - API-key authentication outcomes on a protected endpoint: 401 for a
//!   missing, unknown, or revoked key and success for a valid active key
//!   (Req 3.1, 3.2, 3.3, 3.4).
//! - Valid send acceptance: `POST /api/v1/messages` with a valid key returns
//!   `202 Accepted` with a `queued` body (Req 1.1).
//! - Inbound listing with a valid key returns `200 OK` with a JSON array
//!   (Req 2.4, exercised via the authenticated path).
//! - The `/health` response shape (Req 9.1) and the `/status` response shape,
//!   including per-command unavailable-field reporting (Req 9.2, 9.7).
//!
//! Because the in-crate `Db::connect_in_memory` helper is `#[cfg(test)]` and
//! therefore not visible to this external integration-test crate, each test
//! provisions a fresh on-disk SQLite database in the system temp directory and
//! removes it (best effort) when the test's [`TempDb`] guard drops.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use serde_json::Value;
use tower::util::ServiceExt; // for `oneshot`

use sms_micro_service::api::{ApiState, ModemPort, router};
use sms_micro_service::auth::key_identifier;
use sms_micro_service::db::Db;
use sms_micro_service::health::{ModemStatusSnapshot, SimStatus};
use sms_micro_service::models::MessageStatus;
use sms_micro_service::modem::SendResult;

// ---------------------------------------------------------------------------
// Stub modem
// ---------------------------------------------------------------------------

/// A stub Modem Manager port with a fixed status snapshot and a canned send
/// result, so the API layer can be exercised without real hardware. The send
/// always resolves to the configured [`SendResult`].
#[derive(Clone)]
struct StubModem {
    snapshot: ModemStatusSnapshot,
    result: SendResult,
}

impl ModemPort for StubModem {
    fn status_snapshot(&self) -> ModemStatusSnapshot {
        self.snapshot.clone()
    }

    async fn send(&self, _to: String, _body: String) -> SendResult {
        self.result.clone()
    }
}

/// A fully healthy/deliverable snapshot: serial up, SIM ready, registered,
/// responsive, with signal and operator known.
fn healthy_snapshot() -> ModemStatusSnapshot {
    ModemStatusSnapshot {
        serial_connected: true,
        sim_status: SimStatus::Ready,
        registered: true,
        responsive: true,
        signal_percent: Some(80),
        operator: Some("Carrier".to_string()),
    }
}

/// A snapshot whose `AT+CSQ`, `AT+CREG?`, and `AT+COPS?` values are all
/// unavailable: signal and operator are absent and the modem is unresponsive
/// (so registration cannot be reported). Used to exercise the `/status`
/// unavailable-field reporting (Req 9.7).
fn unavailable_snapshot() -> ModemStatusSnapshot {
    ModemStatusSnapshot {
        serial_connected: true,
        sim_status: SimStatus::Ready,
        registered: true,
        responsive: false,
        signal_percent: None,
        operator: None,
    }
}

/// A send result that leaves the outbound record `queued` (delivery
/// preconditions deferred, Req 10.5), so the persisted state after the
/// background dispatch is deterministic for assertions.
fn queued_result() -> SendResult {
    SendResult {
        status: MessageStatus::Queued,
        reference: None,
        error_code: None,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Temp-file database harness
// ---------------------------------------------------------------------------

/// A migrated on-disk SQLite database plus the paths to remove on drop.
struct TempDb {
    db: Db,
    base: PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        // Best-effort cleanup of the database file and its WAL/SHM siblings.
        let _ = std::fs::remove_file(&self.base);
        for suffix in ["-wal", "-shm"] {
            let mut p = self.base.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

/// Allocate a unique temp database path for this process.
fn temp_db_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sms_api_it_{}_{}_{}.sqlite",
        std::process::id(),
        nanos,
        n
    ))
}

/// Connect to a fresh temp-file database and run migrations, returning a
/// ready handle wrapped in a cleanup guard.
async fn ready_db() -> TempDb {
    let base = temp_db_path();
    let db = Db::initialize(base.to_str().expect("temp path is valid UTF-8"))
        .await
        .expect("initialize temp database");
    TempDb { db, base }
}

/// Insert an active, non-revoked API key keyed by the non-reversible
/// identifier of `plaintext`, returning its row id.
async fn insert_key(db: &Db, plaintext: &str) -> i64 {
    let ident = key_identifier(plaintext);
    let hash = format!("hash-{ident}");
    let result = sqlx::query(
        "INSERT INTO api_keys (key_hash, key_identifier, custom_rate_limit, revoked, created_at) \
         VALUES (?, ?, NULL, 0, ?)",
    )
    .bind(&hash)
    .bind(&ident)
    .bind(Utc::now().to_rfc3339())
    .execute(db.pool())
    .await
    .expect("insert api key");
    result.last_insert_rowid()
}

/// Mark the API key with the given row id as revoked.
async fn revoke_key(db: &Db, id: i64) {
    sqlx::query("UPDATE api_keys SET revoked = 1 WHERE id = ?")
        .bind(id)
        .execute(db.pool())
        .await
        .expect("revoke api key");
}

/// Build the assembled router over a stub modem with the given snapshot and
/// the deterministic `queued` send result.
fn app_with(db: Db, snapshot: ModemStatusSnapshot) -> axum::Router {
    let modem = StubModem {
        snapshot,
        result: queued_result(),
    };
    router(ApiState::new(db, modem))
}

/// Build a `GET` request for `path`, optionally carrying an `X-API-Key`.
fn get_request(path: &str, api_key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    builder.body(Body::empty()).unwrap()
}

/// Read the full response body and parse it as JSON.
async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("response body is valid JSON")
}

// ---------------------------------------------------------------------------
// Authentication outcomes (Req 3.1–3.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn protected_endpoint_missing_key_returns_401() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let resp = app
        .oneshot(get_request("/api/v1/messages/inbound", None))
        .await
        .unwrap();

    // Missing key: rejected with 401 and no business processing (Req 3.2).
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_endpoint_unknown_key_returns_401() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let resp = app
        .oneshot(get_request("/api/v1/messages/inbound", Some("no-such-key")))
        .await
        .unwrap();

    // Unknown key: rejected with 401 (Req 3.3).
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_endpoint_revoked_key_returns_401() {
    let temp = ready_db().await;
    let id = insert_key(&temp.db, "revoked-key").await;
    revoke_key(&temp.db, id).await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let resp = app
        .oneshot(get_request("/api/v1/messages/inbound", Some("revoked-key")))
        .await
        .unwrap();

    // Revoked key: rejected with 401 exactly like an unknown key (Req 3.4).
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn protected_endpoint_valid_key_succeeds() {
    let temp = ready_db().await;
    insert_key(&temp.db, "good-key").await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let resp = app
        .oneshot(get_request("/api/v1/messages/inbound", Some("good-key")))
        .await
        .unwrap();

    // Valid active key: the request is authorized and processed (Req 3.1).
    assert_eq!(resp.status(), StatusCode::OK);
}

// ---------------------------------------------------------------------------
// Valid send acceptance (Req 1.1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn valid_send_returns_202_with_queued_body() {
    let temp = ready_db().await;
    insert_key(&temp.db, "send-key").await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/messages")
        .header("x-api-key", "send-key")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"to":"+14155552671","body":"hello world"}"#))
        .unwrap();

    let resp = app.oneshot(request).await.unwrap();

    // The send is accepted and a queued record is created (Req 1.1).
    assert_eq!(resp.status(), StatusCode::ACCEPTED);

    let body = json_body(resp).await;
    assert_eq!(body["status"], "queued");
    assert_eq!(body["parts"], 1);
    assert!(
        body["id"].as_i64().is_some(),
        "acceptance body carries a numeric id, got {body:?}"
    );
}

// ---------------------------------------------------------------------------
// Inbound listing with a valid key (Req 2.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inbound_listing_with_valid_key_returns_array() {
    let temp = ready_db().await;
    insert_key(&temp.db, "list-key").await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let resp = app
        .oneshot(get_request("/api/v1/messages/inbound", Some("list-key")))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = json_body(resp).await;
    // With no persisted inbound messages the listing is an empty JSON array.
    assert!(body.is_array(), "inbound listing must be a JSON array");
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// ---------------------------------------------------------------------------
// Health (Req 9.1) and status (Req 9.2, 9.7) response shapes
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_response_shape_when_healthy() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    // `/health` is unauthenticated (Req 9.1).
    let resp = app
        .oneshot(get_request("/health", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = json_body(resp).await;
    // The health response reports the overall verdict, the serial connection
    // state, and the SIM status (Req 9.1).
    assert_eq!(body["health"], "healthy");
    assert_eq!(body["serial_connected"], true);
    assert_eq!(body["sim_status"], "ready");
}

#[tokio::test]
async fn status_response_shape_when_all_values_available() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    // `/status` is unauthenticated (Req 9.2).
    let resp = app
        .oneshot(get_request("/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = json_body(resp).await;
    assert_eq!(body["signal_percent"], 80);
    assert_eq!(body["registered"], true);
    assert_eq!(body["operator"], "Carrier");
    // Nothing is unavailable when every modem command responded (Req 9.2).
    assert_eq!(body["unavailable"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn status_response_reports_unavailable_fields() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), unavailable_snapshot());

    let resp = app
        .oneshot(get_request("/status", None))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    let body = json_body(resp).await;
    // Each command that did not respond is reported as null and named in the
    // `unavailable` list (Req 9.7).
    assert!(body["signal_percent"].is_null());
    assert!(body["registered"].is_null());
    assert!(body["operator"].is_null());

    let unavailable: Vec<String> = body["unavailable"]
        .as_array()
        .expect("unavailable is an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    assert!(unavailable.contains(&"signal".to_string()));
    assert!(unavailable.contains(&"registration".to_string()));
    assert!(unavailable.contains(&"operator".to_string()));
}

// ---------------------------------------------------------------------------
// Unauthenticated operational endpoints are reachable without a key
// ---------------------------------------------------------------------------

#[tokio::test]
async fn health_and_status_do_not_require_a_key() {
    let temp = ready_db().await;
    let app = app_with(temp.db.clone(), healthy_snapshot());

    let health = app
        .clone()
        .oneshot(get_request("/health", None))
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);

    let status = app
        .oneshot(get_request("/status", None))
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
}
