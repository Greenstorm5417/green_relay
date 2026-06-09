//! API key authentication: hashing, pre-lookup guard, lockout, and
//! `authenticate`.
//!
//! This module holds the authentication primitives for the REST API
//! (task 8.1). All credential handling here is non-reversible: a presented
//! key is only ever turned into a SHA-256 [`key_identifier`] that is safe to
//! log and audit; the plaintext key is never stored, logged, or returned.
//!
//! The pieces are:
//! - [`key_identifier`] — the SHA-256 hex identifier of a presented key
//!   (Req 3.5).
//! - [`passes_guard`] — the pre-lookup guard that rejects empty or
//!   over-length keys before any store lookup (Req 3.7).
//! - [`FailureTracker`] / [`is_locked_out`] — per-identifier failure tracking
//!   with lockout: 5 failures within a trailing 60-second window locks the
//!   identifier out for 300 seconds (Req 3.8).
//! - [`authenticate`] — ties the guard, lockout, and store lookup together,
//!   returning an [`AuthOutcome`] (Req 3.1–3.4, 3.7, 3.8).
//! - [`AuthAuditRecord`] / [`build_audit_record`] — the audit/log record for
//!   an auth attempt, containing only the non-reversible identifier and never
//!   the plaintext credential (Req 3.6, 7.6).
//!
//! The DB-backed [`KeyStore`] lookup and the async Axum middleware that wraps
//! these primitives are wired up later (task 13.2); here the store is an
//! abstraction so the auth logic stays pure and testable.
//!
//! Validates: Requirements 3.1, 3.2, 3.3, 3.4, 3.5, 3.6, 3.7, 3.8

use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Identifier of a stored API key (its database primary key).
pub type ApiKeyId = i64;

/// Maximum length, in characters, of an acceptable presented key (Req 3.7).
///
/// Keys longer than this are rejected before any store lookup.
pub const MAX_KEY_LEN: usize = 256;

/// Number of failures within the trailing window that triggers a lockout
/// (Req 3.8).
pub const LOCKOUT_FAILURE_THRESHOLD: usize = 5;

/// Trailing window over which failures are counted for lockout (Req 3.8).
pub const LOCKOUT_WINDOW: Duration = Duration::from_secs(60);

/// Duration an identifier remains locked out once triggered (Req 3.8).
pub const LOCKOUT_DURATION: Duration = Duration::from_secs(300);

/// Outcome of an authentication attempt.
///
/// Drives both the audit log entry and the HTTP status returned by the auth
/// middleware: [`Authorized`](AuthOutcome::Authorized) proceeds, while both
/// [`Unauthorized`](AuthOutcome::Unauthorized) and
/// [`LockedOut`](AuthOutcome::LockedOut) map to HTTP 401 with no business
/// processing (Req 3.2, 3.3, 3.4, 3.7, 3.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// The presented key matched an active, non-revoked stored key.
    Authorized(ApiKeyId),
    /// The key was absent, empty, over-length, unknown, or revoked.
    Unauthorized,
    /// The identifier is currently locked out due to repeated failures.
    LockedOut,
}

/// Abstraction over the active-key store used by [`authenticate`].
///
/// An implementation looks up an active, non-revoked API key by its
/// non-reversible [`key_identifier`] and returns the key's [`ApiKeyId`] when
/// present. The DB-backed implementation is provided when the persistence
/// layer is wired into the auth middleware (task 13.2); keeping this as a
/// trait lets the auth logic be exercised in isolation.
pub trait KeyStore {
    /// Return the [`ApiKeyId`] for an active, non-revoked key whose identifier
    /// equals `key_identifier`, or `None` if no such key exists (covering
    /// unknown and revoked keys alike — Req 3.3, 3.4).
    fn lookup_active(&self, key_identifier: &str) -> Option<ApiKeyId>;
}

/// Compute the non-reversible identifier of a presented key.
///
/// This is the lowercase hex encoding of the SHA-256 digest of the key bytes.
/// It is deterministic, fixed-length (64 hex characters), and never equal to
/// the plaintext key, which makes it safe to store, index, log, and audit
/// (Req 3.5, 3.6).
pub fn key_identifier(presented_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(presented_key.as_bytes());
    to_hex(&hasher.finalize())
}

/// Lowercase hex-encode a byte slice.
///
/// Uses a direct nibble lookup table rather than `write!(.., "{:02x}")` per
/// byte: the formatting machinery dominated `key_identifier`, which runs on
/// every authenticated request. Each output byte is an ASCII hex digit, so the
/// assembled buffer is valid UTF-8 by construction.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize]);
        out.push(HEX[(b & 0x0f) as usize]);
    }
    // SAFETY: every pushed byte is an ASCII hex digit (`0-9a-f`), so `out` is
    // guaranteed to be valid UTF-8.
    unsafe { String::from_utf8_unchecked(out) }
}

/// Pre-lookup guard for a presented key.
///
/// Returns `true` only when the key is acceptable for a store lookup: it must
/// be non-empty and no longer than [`MAX_KEY_LEN`] characters. Empty or
/// over-length keys are rejected here so that no key lookup is performed for
/// them (Req 3.7).
pub fn passes_guard(presented: &str) -> bool {
    let len = presented.chars().count();
    len >= 1 && len <= MAX_KEY_LEN
}

/// Compute the instant until which a timeline of failures keeps an identifier
/// locked out, if any.
///
/// For each failure, the number of failures within the trailing
/// [`LOCKOUT_WINDOW`] ending at that failure is counted; when that count
/// reaches [`LOCKOUT_FAILURE_THRESHOLD`], a lockout is triggered that expires
/// [`LOCKOUT_DURATION`] after the triggering failure. The returned value is
/// the latest such expiry, or `None` if no window ever reached the threshold.
///
/// The trailing window is treated as closed: a failure at time `t` counts
/// toward the window anchored at `anchor` when `anchor - t <= LOCKOUT_WINDOW`.
pub fn lockout_until(failures: &[Instant]) -> Option<Instant> {
    let mut sorted: Vec<Instant> = failures.to_vec();
    sorted.sort_unstable();

    let mut result: Option<Instant> = None;
    for i in 0..sorted.len() {
        let anchor = sorted[i];
        // Count failures at or before `anchor` that fall within the trailing
        // window. Using saturating_duration_since avoids any Instant
        // underflow and treats out-of-order inputs safely.
        let count = sorted[..=i]
            .iter()
            .filter(|t| anchor.saturating_duration_since(**t) <= LOCKOUT_WINDOW)
            .count();
        if count >= LOCKOUT_FAILURE_THRESHOLD {
            let until = anchor + LOCKOUT_DURATION;
            result = Some(result.map_or(until, |r| r.max(until)));
        }
    }
    result
}

/// Return whether a timeline of failures leaves an identifier locked out at
/// `now`.
///
/// True iff [`lockout_until`] yields an expiry strictly after `now` (Req 3.8).
pub fn is_locked_out(failures: &[Instant], now: Instant) -> bool {
    lockout_until(failures).is_some_and(|until| now < until)
}

/// Per-identifier authentication failure tracker.
///
/// Records the instants of failed authentication attempts for each key
/// identifier and derives the lockout state from them via [`is_locked_out`].
/// A successful authentication clears an identifier's failure history.
#[derive(Debug, Default)]
pub struct FailureTracker {
    failures: HashMap<String, Vec<Instant>>,
}

impl FailureTracker {
    /// Create an empty tracker.
    pub fn new() -> Self {
        FailureTracker {
            failures: HashMap::new(),
        }
    }

    /// Whether `identifier` is currently locked out at `now` (Req 3.8).
    pub fn is_locked(&self, identifier: &str, now: Instant) -> bool {
        self.failures
            .get(identifier)
            .is_some_and(|f| is_locked_out(f, now))
    }

    /// Record a failed authentication attempt for `identifier` at `now`.
    ///
    /// Old failures that can no longer influence any future lockout decision
    /// (older than the combined window plus lockout duration relative to
    /// `now`) are pruned to keep the history bounded.
    pub fn record_failure(&mut self, identifier: &str, now: Instant) {
        let history = self.failures.entry(identifier.to_string()).or_default();
        history.push(now);

        // A failure can only matter while it could still anchor a window whose
        // lockout has not expired. Anything older than window + lockout
        // relative to `now` is irrelevant and can be dropped.
        let horizon = LOCKOUT_WINDOW + LOCKOUT_DURATION;
        history.retain(|t| now.saturating_duration_since(*t) <= horizon);
    }

    /// Clear the failure history for `identifier` after a success (Req 3.1).
    pub fn record_success(&mut self, identifier: &str) {
        self.failures.remove(identifier);
    }
}

/// Authenticate a presented API key against `store`, updating `tracker`.
///
/// The sequence mirrors the design's request lifecycle:
/// 1. Reject empty or over-length keys before any lookup (Req 3.7).
/// 2. Reject when the identifier is locked out (Req 3.8).
/// 3. Look up the active, non-revoked key; on a match clear the failure
///    history and authorize (Req 3.1); otherwise record the failure and
///    reject (Req 3.2, 3.3, 3.4).
///
/// The plaintext `presented` key is never stored or returned — only its
/// non-reversible [`key_identifier`] is used internally and for tracking.
pub fn authenticate<S: KeyStore + ?Sized>(
    presented: &str,
    store: &S,
    tracker: &mut FailureTracker,
    now: Instant,
) -> AuthOutcome {
    // Pre-lookup guard: empty or over-length keys never reach the store.
    if !passes_guard(presented) {
        return AuthOutcome::Unauthorized;
    }

    let identifier = key_identifier(presented);
    authenticate_identified(&identifier, store, tracker, now)
}

/// Authenticate using an already-computed key [`key_identifier`].
///
/// This is the identifier-keyed core of [`authenticate`]: it performs the
/// lockout check and the active-key lookup without recomputing the SHA-256
/// identifier. The request hot path (the auth middleware) derives the
/// identifier exactly once — for the pre-lookup guard and the lockout check —
/// then reuses it here and for the audit record, avoiding the repeated hashing
/// that recomputing from the plaintext would incur (Req 3.1, 3.2, 3.3, 3.4,
/// 3.8).
///
/// The caller is responsible for having applied [`passes_guard`] to the
/// presented key before deriving the identifier.
pub fn authenticate_identified<S: KeyStore + ?Sized>(
    identifier: &str,
    store: &S,
    tracker: &mut FailureTracker,
    now: Instant,
) -> AuthOutcome {
    // Lockout takes precedence over any lookup so a locked-out identifier
    // performs no business processing.
    if tracker.is_locked(identifier, now) {
        return AuthOutcome::LockedOut;
    }

    match store.lookup_active(identifier) {
        Some(id) => {
            tracker.record_success(identifier);
            AuthOutcome::Authorized(id)
        }
        None => {
            tracker.record_failure(identifier, now);
            AuthOutcome::Unauthorized
        }
    }
}

/// The validation result recorded for an authentication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthResult {
    /// The attempt was authorized.
    Authorized,
    /// The attempt was rejected (absent, unknown, revoked, or guarded out).
    Unauthorized,
    /// The attempt was rejected because the identifier is locked out.
    LockedOut,
}

impl From<&AuthOutcome> for AuthResult {
    fn from(outcome: &AuthOutcome) -> Self {
        match outcome {
            AuthOutcome::Authorized(_) => AuthResult::Authorized,
            AuthOutcome::Unauthorized => AuthResult::Unauthorized,
            AuthOutcome::LockedOut => AuthResult::LockedOut,
        }
    }
}

/// An audit/log record describing the outcome of an authentication attempt.
///
/// Contains the validation result, a timestamp, and the non-reversible key
/// identifier — and deliberately never the plaintext key (Req 3.6, 7.6). The
/// record derives `Serialize` so it can be written to the audit log and
/// emitted as a structured log event without further transformation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthAuditRecord {
    /// Event category; always `"api_key_auth"` for these records.
    pub event_type: &'static str,
    /// The validation result.
    pub result: AuthResult,
    /// Non-reversible SHA-256 identifier of the presented key (never plaintext).
    pub key_identifier: String,
    /// When the attempt completed, in UTC.
    pub timestamp: DateTime<Utc>,
}

/// Build the audit/log record for an authentication attempt.
///
/// The record carries only the non-reversible [`key_identifier`] of
/// `presented`, never the plaintext key, satisfying the "no plaintext in
/// audit or logs" requirement (Req 3.5, 3.6, 7.6).
pub fn build_audit_record(
    presented: &str,
    outcome: &AuthOutcome,
    timestamp: DateTime<Utc>,
) -> AuthAuditRecord {
    build_audit_record_with_identifier(key_identifier(presented), outcome, timestamp)
}

/// Build the audit/log record from an already-computed key [`key_identifier`].
///
/// The identifier-keyed counterpart to [`build_audit_record`], letting the
/// request hot path reuse the identifier it already derived instead of hashing
/// the plaintext key again (Req 3.5, 3.6, 7.6).
pub fn build_audit_record_with_identifier(
    key_identifier: String,
    outcome: &AuthOutcome,
    timestamp: DateTime<Utc>,
) -> AuthAuditRecord {
    AuthAuditRecord {
        event_type: "api_key_auth",
        result: AuthResult::from(outcome),
        key_identifier,
        timestamp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Simple in-memory key store backed by a set of active identifiers.
    struct MapStore {
        active: HashMap<String, ApiKeyId>,
    }

    impl MapStore {
        fn with_key(plaintext: &str, id: ApiKeyId) -> Self {
            let mut active = HashMap::new();
            active.insert(key_identifier(plaintext), id);
            MapStore { active }
        }

        fn empty() -> Self {
            MapStore {
                active: HashMap::new(),
            }
        }
    }

    impl KeyStore for MapStore {
        fn lookup_active(&self, key_identifier: &str) -> Option<ApiKeyId> {
            self.active.get(key_identifier).copied()
        }
    }

    #[test]
    fn key_identifier_is_deterministic_fixed_length_and_non_reversible() {
        let id1 = key_identifier("super-secret-key");
        let id2 = key_identifier("super-secret-key");
        assert_eq!(id1, id2, "hashing must be deterministic");
        assert_eq!(id1.len(), 64, "SHA-256 hex is 64 characters");
        assert!(id1.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(id1, "super-secret-key", "identifier is never the plaintext");
        assert_ne!(key_identifier("a"), key_identifier("b"));
    }

    #[test]
    fn key_identifier_matches_known_sha256_vector() {
        // SHA-256("") well-known digest.
        assert_eq!(
            key_identifier(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn guard_rejects_empty_and_overlong_keys_only() {
        assert!(!passes_guard(""));
        assert!(passes_guard("k"));
        assert!(passes_guard(&"k".repeat(MAX_KEY_LEN)));
        assert!(!passes_guard(&"k".repeat(MAX_KEY_LEN + 1)));
    }

    #[test]
    fn authenticate_authorizes_matching_active_key() {
        let store = MapStore::with_key("good-key", 7);
        let mut tracker = FailureTracker::new();
        let now = Instant::now();
        assert_eq!(
            authenticate("good-key", &store, &mut tracker, now),
            AuthOutcome::Authorized(7)
        );
    }

    #[test]
    fn authenticate_rejects_unknown_key_without_locking_on_first_failure() {
        let store = MapStore::empty();
        let mut tracker = FailureTracker::new();
        let now = Instant::now();
        assert_eq!(
            authenticate("nope", &store, &mut tracker, now),
            AuthOutcome::Unauthorized
        );
    }

    #[test]
    fn authenticate_guards_out_empty_and_overlong_without_lookup() {
        let store = MapStore::empty();
        let mut tracker = FailureTracker::new();
        let now = Instant::now();
        assert_eq!(
            authenticate("", &store, &mut tracker, now),
            AuthOutcome::Unauthorized
        );
        let overlong = "x".repeat(MAX_KEY_LEN + 1);
        assert_eq!(
            authenticate(&overlong, &store, &mut tracker, now),
            AuthOutcome::Unauthorized
        );
        // Guarded-out attempts are not recorded as failures.
        assert!(tracker.failures.is_empty());
    }

    #[test]
    fn five_failures_within_window_lock_out_for_300s() {
        let store = MapStore::empty();
        let mut tracker = FailureTracker::new();
        let base = Instant::now();

        // Four failures: still not locked.
        for i in 0..4 {
            let now = base + Duration::from_secs(i * 10);
            assert_eq!(
                authenticate("bad", &store, &mut tracker, now),
                AuthOutcome::Unauthorized
            );
        }

        // Fifth failure within the 60s window triggers the lockout.
        let trigger = base + Duration::from_secs(40);
        assert_eq!(
            authenticate("bad", &store, &mut tracker, trigger),
            AuthOutcome::Unauthorized
        );

        // Subsequent attempts are locked out for 300s from the trigger.
        let during = trigger + Duration::from_secs(299);
        assert_eq!(
            authenticate("bad", &store, &mut tracker, during),
            AuthOutcome::LockedOut
        );

        // After 300s the lockout has expired.
        let after = trigger + Duration::from_secs(301);
        assert!(!tracker.is_locked(&key_identifier("bad"), after));
    }

    #[test]
    fn failures_outside_window_do_not_lock_out() {
        let mut times = Vec::new();
        let base = Instant::now();
        // Five failures spread over 5 minutes: never 5 within any 60s window.
        for i in 0..5 {
            times.push(base + Duration::from_secs(i * 70));
        }
        assert!(lockout_until(&times).is_none());
        assert!(!is_locked_out(&times, base + Duration::from_secs(1000)));
    }

    #[test]
    fn lockout_until_returns_trigger_plus_duration() {
        let base = Instant::now();
        let times: Vec<Instant> = (0..5).map(|i| base + Duration::from_secs(i * 5)).collect();
        // Threshold reached at the fifth failure (t = base + 20s).
        let expected = base + Duration::from_secs(20) + LOCKOUT_DURATION;
        assert_eq!(lockout_until(&times), Some(expected));
    }

    #[test]
    fn success_clears_failure_history() {
        let store = MapStore::with_key("good", 1);
        let mut tracker = FailureTracker::new();
        let base = Instant::now();
        // Accumulate four failures on the same identifier.
        let id = key_identifier("good");
        for i in 0..4 {
            tracker.record_failure(&id, base + Duration::from_secs(i));
        }
        // A success clears them, so a later lockout cannot fire from stale data.
        assert_eq!(
            authenticate("good", &store, &mut tracker, base + Duration::from_secs(5)),
            AuthOutcome::Authorized(1)
        );
        assert!(!tracker.failures.contains_key(&id));
    }

    #[test]
    fn audit_record_contains_identifier_and_never_plaintext() {
        let plaintext = "top-secret-credential-value";
        let outcome = AuthOutcome::Unauthorized;
        let record = build_audit_record(plaintext, &outcome, Utc::now());

        assert_eq!(record.event_type, "api_key_auth");
        assert_eq!(record.result, AuthResult::Unauthorized);
        assert_eq!(record.key_identifier, key_identifier(plaintext));

        // The serialized record must not leak the plaintext credential.
        let json = serde_json::to_string(&record).unwrap();
        assert!(!json.contains(plaintext), "audit record leaked plaintext");
    }

    #[test]
    fn audit_record_result_tracks_outcome() {
        assert_eq!(
            build_audit_record("k", &AuthOutcome::Authorized(3), Utc::now()).result,
            AuthResult::Authorized
        );
        assert_eq!(
            build_audit_record("k", &AuthOutcome::LockedOut, Utc::now()).result,
            AuthResult::LockedOut
        );
    }

    #[test]
    fn distinct_keys_have_distinct_identifiers() {
        let ids: HashSet<String> = ["a", "b", "c", "aa", "ab"]
            .iter()
            .map(|k| key_identifier(k))
            .collect();
        assert_eq!(ids.len(), 5);
    }
}
