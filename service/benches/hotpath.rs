//! Hot-path micro-benchmarks (no-admin profiling via Criterion).
//!
//! These benchmarks target the per-request work the service does on its
//! latency-sensitive paths — API-key authentication, rate limiting, request
//! validation, SMS segmentation, and AT-response parsing — so optimizations
//! can be measured rather than guessed. Run with `cargo bench`.
//!
//! The auth benchmark models exactly what the auth middleware does for one
//! request (`auth_request_flow`) so the cost of computing the key identifier
//! and building the audit record is captured end to end.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use green_relay::auth::{
    ApiKeyId, FailureTracker, KeyStore, authenticate, authenticate_identified, build_audit_record,
    build_audit_record_with_identifier, key_identifier,
};
use green_relay::health::{ModemStatusSnapshot, SimStatus, derive_health};
use green_relay::logging::{LogRecord, request_log};
use green_relay::modem::parse_send_outcome;
use green_relay::ratelimit::RateLimiter;
use green_relay::sms::{segment_message, validate_body, validate_e164};

/// A trivial key store that always resolves the same active key id, mirroring
/// the "valid key" hot path through the auth middleware.
struct AlwaysActive(ApiKeyId);

impl KeyStore for AlwaysActive {
    fn lookup_active(&self, _identifier: &str) -> Option<ApiKeyId> {
        Some(self.0)
    }
}

/// A representative high-entropy API key sample, shaped like the keys the admin
/// panel issues. The `grk_sample_` prefix keeps it from resembling any real
/// provider's secret-key format.
const SAMPLE_KEY: &str = "grk_sample_3f8a9c2e1b7d4655a0c9f2e8b1d6473a9e5c8f2a1b3d6e7f";

fn bench_key_identifier(c: &mut Criterion) {
    c.bench_function("key_identifier", |b| {
        b.iter(|| key_identifier(black_box(SAMPLE_KEY)))
    });
}

fn bench_auth_request_flow(c: &mut Criterion) {
    let store = AlwaysActive(7);

    // Optimized path: exactly what the auth middleware now does — derive the
    // identifier once, run the identifier-keyed authenticate core, and build
    // the audit record from that same identifier (one SHA-256 per request).
    c.bench_function("auth_request_flow_optimized", |b| {
        b.iter(|| {
            let mut tracker = FailureTracker::new();
            let now = Instant::now();
            let presented = black_box(SAMPLE_KEY);
            let identifier = key_identifier(presented);
            let outcome = authenticate_identified(&identifier, &store, &mut tracker, now);
            let record =
                build_audit_record_with_identifier(identifier, &outcome, chrono::Utc::now());
            black_box(record)
        })
    });

    // Naive path: the pre-optimization shape that recomputed the SHA-256
    // identifier three times per request (explicit derive + inside
    // `authenticate` + inside `build_audit_record`). Kept as a comparison
    // point to quantify the optimization.
    c.bench_function("auth_request_flow_naive", |b| {
        b.iter(|| {
            let mut tracker = FailureTracker::new();
            let now = Instant::now();
            let presented = black_box(SAMPLE_KEY);
            let _identifier = key_identifier(presented);
            let outcome = authenticate(presented, &store, &mut tracker, now);
            let record = build_audit_record(presented, &outcome, chrono::Utc::now());
            black_box(record)
        })
    });
}

fn bench_rate_limiter_check(c: &mut Criterion) {
    // Steady-state: the key already exists, which is the common case on a
    // busy key. Measures the per-request limiter cost.
    let mut limiter = RateLimiter::new();
    let window = Duration::from_secs(60);
    let key = key_identifier(SAMPLE_KEY);
    limiter.check(&key, 1_000_000, window, Instant::now());
    c.bench_function("rate_limiter_check_existing_key", |b| {
        b.iter(|| black_box(limiter.check(black_box(&key), 1_000_000, window, Instant::now())))
    });
}

fn bench_validation(c: &mut Criterion) {
    c.bench_function("validate_request", |b| {
        b.iter(|| {
            let _ = black_box(validate_e164(black_box("+14155552671")));
            let _ = black_box(validate_body(black_box(
                "hello world, this is a test message",
            )));
        })
    });
}

fn bench_segment_single(c: &mut Criterion) {
    let body = "hello world, this is a single-part text message under 160 chars";
    c.bench_function("segment_single_part", |b| {
        b.iter(|| black_box(segment_message(black_box(body))))
    });
}

fn bench_segment_multi(c: &mut Criterion) {
    let body = "x".repeat(1530);
    c.bench_function("segment_ten_parts", |b| {
        b.iter(|| black_box(segment_message(black_box(&body))))
    });
}

fn bench_parse_send_outcome(c: &mut Criterion) {
    let lines = ["AT+CMGS=\"+14155552671\"", "+CMGS: 42", "OK"];
    c.bench_function("parse_send_outcome", |b| {
        b.iter(|| black_box(parse_send_outcome(black_box(&lines))))
    });
}

fn bench_derive_health(c: &mut Criterion) {
    let snapshot = ModemStatusSnapshot {
        serial_connected: true,
        sim_status: SimStatus::Ready,
        registered: true,
        responsive: true,
        signal_percent: Some(80),
        operator: Some("Carrier".to_string()),
    };
    c.bench_function("derive_health", |b| {
        b.iter(|| black_box(derive_health(black_box(&snapshot))))
    });
}

fn bench_log_now_timestamp(c: &mut Criterion) {
    c.bench_function("log_now_timestamp", |b| {
        b.iter(|| black_box(LogRecord::now_timestamp()))
    });
}

fn bench_log_build_request(c: &mut Criterion) {
    c.bench_function("log_build_request", |b| {
        b.iter(|| {
            black_box(request_log(
                black_box("POST"),
                black_box("/api/v1/messages"),
                black_box(202),
            ))
        })
    });
}

fn bench_log_to_json_string(c: &mut Criterion) {
    let record = request_log("POST", "/api/v1/messages", 202);
    c.bench_function("log_to_json_string", |b| {
        b.iter(|| black_box(record.to_json_string()))
    });
}

criterion_group!(
    hotpath,
    bench_key_identifier,
    bench_auth_request_flow,
    bench_rate_limiter_check,
    bench_validation,
    bench_segment_single,
    bench_segment_multi,
    bench_parse_send_outcome,
    bench_derive_health,
    bench_log_now_timestamp,
    bench_log_build_request,
    bench_log_to_json_string,
);
criterion_main!(hotpath);
