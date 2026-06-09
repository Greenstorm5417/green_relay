//! End-to-end flow benchmarks (whole-pipeline profiling via Criterion).
//!
//! Where `hotpath.rs` measures individual latency-sensitive functions in
//! isolation, this benchmark drives the *fully assembled* Axum router built by
//! [`sms_micro_service::api::router`] through `tower`'s
//! [`ServiceExt::oneshot`], so every layer a real request crosses is included:
//! API-key authentication (with the SQLite key lookup), the per-key rate-limit
//! layer, request validation, SMS segmentation, the `queued` outbound DB
//! insert, and the background-dispatch spawn — plus the unauthenticated
//! `/health` and `/status` read paths.
//!
//! It uses a migrated on-disk SQLite database in the system temp directory and
//! a stub Modem Manager port (no real hardware), exactly like the HTTP
//! integration tests. Run with `cargo bench`.

use std::hint::black_box;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use chrono::Utc;
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::runtime::Runtime;
use tower::util::ServiceExt; // for `oneshot`

use sms_micro_service::api::{ApiState, ModemPort, router};
use sms_micro_service::auth::key_identifier;
use sms_micro_service::db::Db;
use sms_micro_service::health::{ModemStatusSnapshot, SimStatus};
use sms_micro_service::models::MessageStatus;
use sms_micro_service::modem::SendResult;

// ---------------------------------------------------------------------------
// Stub modem (no hardware)
// ---------------------------------------------------------------------------

/// A stub Modem Manager port: a fixed healthy snapshot and a canned `queued`
/// send result, so the full request flow runs without real hardware.
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

/// A send result that leaves the record `queued`, so the background dispatch is
/// deterministic and does not depend on a modem reply.
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

/// Allocate a unique temp database path for this process.
fn temp_db_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "sms_fullflow_{}_{nanos}_{n}.sqlite",
        std::process::id()
    ))
}

/// Connect to a fresh temp-file database and run migrations.
async fn ready_db() -> Db {
    let base = temp_db_path();
    let path = base.to_str().expect("temp path is valid UTF-8");
    Db::initialize(path)
        .await
        .expect("initialize temp database")
}

/// Insert an active, non-revoked API key keyed by the identifier of `plaintext`.
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

/// Build the assembled router with a huge rate limit so the benchmark's tight
/// request loop measures the accept path rather than tripping HTTP 429.
fn build_app(db: Db) -> Router {
    let modem = StubModem {
        snapshot: healthy_snapshot(),
        result: queued_result(),
    };
    router(ApiState::new(db, modem).with_rate_config(u32::MAX, Duration::from_secs(60)))
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

const API_KEY: &str = "bench-key";

fn send_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/v1/messages")
        .header("x-api-key", API_KEY)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"to":"+14155552671","body":"hello world"}"#))
        .expect("build send request")
}

fn get_request(path: &str, api_key: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(path);
    if let Some(key) = api_key {
        builder = builder.header("x-api-key", key);
    }
    builder.body(Body::empty()).expect("build GET request")
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Shared multi-threaded runtime so the handler's `tokio::spawn` background
/// dispatch has a worker to run on, mirroring the production runtime.
fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

/// `POST /api/v1/messages` with a valid key: auth + rate limit + validation +
/// segmentation + `queued` insert + dispatch spawn — the full send pipeline.
fn bench_send_flow(c: &mut Criterion) {
    let rt = runtime();
    let app = rt.block_on(async {
        let db = ready_db().await;
        insert_key(&db, API_KEY).await;
        build_app(db)
    });

    c.bench_function("full_flow_send", |b| {
        b.iter(|| {
            let resp = rt
                .block_on(app.clone().oneshot(black_box(send_request())))
                .expect("router responds");
            black_box(resp.status())
        })
    });
}

/// `GET /api/v1/messages/inbound` with a valid key: auth + rate limit + the
/// inbound listing DB query.
fn bench_inbound_list_flow(c: &mut Criterion) {
    let rt = runtime();
    let app = rt.block_on(async {
        let db = ready_db().await;
        insert_key(&db, API_KEY).await;
        build_app(db)
    });

    c.bench_function("full_flow_inbound_list", |b| {
        b.iter(|| {
            let resp = rt
                .block_on(app.clone().oneshot(black_box(get_request(
                    "/api/v1/messages/inbound",
                    Some(API_KEY),
                ))))
                .expect("router responds");
            black_box(resp.status())
        })
    });
}

/// Missing-key rejection: the auth layer's 401 short-circuit (no DB lookup,
/// no business processing).
fn bench_unauthorized_flow(c: &mut Criterion) {
    let rt = runtime();
    let app = rt.block_on(async { build_app(ready_db().await) });

    c.bench_function("full_flow_unauthorized", |b| {
        b.iter(|| {
            let resp = rt
                .block_on(
                    app.clone()
                        .oneshot(black_box(get_request("/api/v1/messages/inbound", None))),
                )
                .expect("router responds");
            black_box(resp.status())
        })
    });
}

/// `GET /health` (unauthenticated): modem snapshot read + health derivation +
/// JSON response.
fn bench_health_flow(c: &mut Criterion) {
    let rt = runtime();
    let app = rt.block_on(async { build_app(ready_db().await) });

    c.bench_function("full_flow_health", |b| {
        b.iter(|| {
            let resp = rt
                .block_on(app.clone().oneshot(black_box(get_request("/health", None))))
                .expect("router responds");
            black_box(resp.status())
        })
    });
}

criterion_group!(
    fullflow,
    bench_send_flow,
    bench_inbound_list_flow,
    bench_unauthorized_flow,
    bench_health_flow,
);
criterion_main!(fullflow);
