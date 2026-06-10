//! HTTP integration tests for the real-time features: the synchronous send
//! endpoint (`POST /api/v1/messages/sync`) and the Server-Sent Events stream
//! (`GET /api/v1/events`).
//!
//! These drive the fully-assembled Axum router through `tower`'s
//! [`ServiceExt::oneshot`] over a fresh temp-file SQLite database, exercising
//! the public HTTP surface end-to-end without real modem hardware.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use chrono::Utc;
use serde_json::Value;
use tokio_stream::StreamExt;
use tower::util::ServiceExt;

use green_relay::api::{ApiState, ModemPort, router};
use green_relay::auth::key_identifier;
use green_relay::db::Db;
use green_relay::events::{EventBus, InboundSmsEvent, ServiceEvent};
use green_relay::health::{ModemStatusSnapshot, SimStatus};
use green_relay::models::MessageStatus;
use green_relay::modem::SendResult;

// ---------------------------------------------------------------------------
// Stub modem
// ---------------------------------------------------------------------------

/// A stub Modem Manager port with a fixed status snapshot and a canned send
/// result that resolves immediately.
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

fn down_snapshot() -> ModemStatusSnapshot {
    ModemStatusSnapshot {
        serial_connected: false,
        sim_status: SimStatus::Unknown,
        registered: false,
        responsive: false,
        signal_percent: None,
        operator: None,
    }
}

fn sent_result(reference: u32) -> SendResult {
    SendResult {
        status: MessageStatus::Sent,
        reference: Some(reference),
        error_code: None,
        error: None,
    }
}

fn failed_result(code: u16) -> SendResult {
    SendResult {
        status: MessageStatus::Failed,
        reference: None,
        error_code: Some(code),
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Temp-file database harness
// ---------------------------------------------------------------------------

struct TempDb {
    db: Db,
    base: PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.base);
        for suffix in ["-wal", "-shm"] {
            let mut p = self.base.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(p));
        }
    }
}

fn temp_db_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "sms_rt_it_{}_{}_{}.sqlite",
        std::process::id(),
        nanos,
        n
    ))
}

async fn ready_db() -> TempDb {
    let base = temp_db_path();
    let db = Db::initialize(base.to_str().expect("temp path is valid UTF-8"))
        .await
        .expect("initialize temp database");
    TempDb { db, base }
}

async fn insert_key(db: &Db, plaintext: &str) {
    let ident = key_identifier(plaintext);
    let hash = format!("hash-{ident}");
    sqlx::query(
        "INSERT INTO api_keys (key_hash, key_identifier, custom_rate_limit, revoked, created_at) \
         VALUES (?, ?, NULL, 0, ?)",
    )
    .bind(&hash)
    .bind(&ident)
    .bind(Utc::now().to_rfc3339())
    .execute(db.pool())
    .await
    .expect("insert api key");
}

fn state_with(db: Db, snapshot: ModemStatusSnapshot, result: SendResult) -> ApiState<StubModem> {
    ApiState::new(db, StubModem { snapshot, result })
}

fn post_sync(api_key: Option<&str>, json_body: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/messages/sync")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    builder.body(Body::from(json_body.to_string())).unwrap()
}

async fn json_body(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("read response body");
    serde_json::from_slice(&bytes).expect("response body is valid JSON")
}

// ---------------------------------------------------------------------------
// Synchronous send endpoint (Pattern 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sync_send_blocks_and_returns_sent_with_reference() {
    let temp = ready_db().await;
    insert_key(&temp.db, "sync-key").await;
    let app = router(state_with(
        temp.db.clone(),
        healthy_snapshot(),
        sent_result(25),
    ));

    let resp = app
        .oneshot(post_sync(
            Some("sync-key"),
            r#"{"to":"+14155552671","body":"hello"}"#,
        ))
        .await
        .unwrap();

    // The synchronous endpoint waits for delivery and returns the terminal
    // status directly with 200 OK.
    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["status"], "sent");
    assert_eq!(body["reference"], "25");
    assert_eq!(body["parts"], 1);
    assert!(body["id"].as_i64().is_some());
}

#[tokio::test]
async fn sync_send_returns_failed_status() {
    let temp = ready_db().await;
    insert_key(&temp.db, "sync-key").await;
    let app = router(state_with(
        temp.db.clone(),
        healthy_snapshot(),
        failed_result(500),
    ));

    let resp = app
        .oneshot(post_sync(
            Some("sync-key"),
            r#"{"to":"+14155552671","body":"hello"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = json_body(resp).await;
    assert_eq!(body["status"], "failed");
    assert!(body["reference"].is_null());
}

#[tokio::test]
async fn sync_send_requires_authentication() {
    let temp = ready_db().await;
    let app = router(state_with(
        temp.db.clone(),
        healthy_snapshot(),
        sent_result(1),
    ));

    let resp = app
        .oneshot(post_sync(None, r#"{"to":"+14155552671","body":"hi"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn sync_send_gated_when_modem_undeliverable() {
    let temp = ready_db().await;
    insert_key(&temp.db, "sync-key").await;
    let app = router(state_with(temp.db.clone(), down_snapshot(), sent_result(1)));

    let resp = app
        .oneshot(post_sync(
            Some("sync-key"),
            r#"{"to":"+14155552671","body":"hi"}"#,
        ))
        .await
        .unwrap();

    // The deliverability gate rejects before persisting, exactly like the
    // asynchronous endpoint.
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn sync_send_rejects_invalid_phone() {
    let temp = ready_db().await;
    insert_key(&temp.db, "sync-key").await;
    let app = router(state_with(
        temp.db.clone(),
        healthy_snapshot(),
        sent_result(1),
    ));

    let resp = app
        .oneshot(post_sync(Some("sync-key"), r#"{"to":"not-e164","body":"hi"}"#))
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

// ---------------------------------------------------------------------------
// Server-Sent Events stream (Pattern 1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn events_endpoint_requires_authentication() {
    let temp = ready_db().await;
    let app = router(state_with(
        temp.db.clone(),
        healthy_snapshot(),
        sent_result(1),
    ));

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn events_endpoint_streams_published_events() {
    let temp = ready_db().await;
    insert_key(&temp.db, "sse-key").await;

    let bus = EventBus::new(16);
    let state = ApiState::new(
        temp.db.clone(),
        StubModem {
            snapshot: healthy_snapshot(),
            result: sent_result(1),
        },
    )
    .with_events(bus.clone());
    let app = router(state);

    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/v1/events")
                .header("x-api-key", "sse-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected an SSE content type, got {content_type:?}"
    );

    // The handler has subscribed by the time the response head is produced, so
    // an event published now is delivered on the open stream.
    bus.publish(ServiceEvent::InboundSms(InboundSmsEvent {
        id: 7,
        from: "+14155550123".to_string(),
        body: "Hey there!".to_string(),
    }));

    let mut stream = resp.into_body().into_data_stream();
    let mut payload = String::new();
    let deadline = Duration::from_secs(5);
    while !payload.contains("inbound_sms") {
        let chunk = tokio::time::timeout(deadline, stream.next())
            .await
            .expect("timed out waiting for an SSE chunk")
            .expect("stream ended before delivering the event")
            .expect("chunk read error");
        payload.push_str(&String::from_utf8_lossy(&chunk));
    }

    // The frame carries the event name and the JSON data payload.
    assert!(payload.contains("event:inbound_sms") || payload.contains("event: inbound_sms"));
    assert!(payload.contains("\"from\":\"+14155550123\""));
    assert!(payload.contains("\"body\":\"Hey there!\""));
}
