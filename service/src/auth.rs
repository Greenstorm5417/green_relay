use std::collections::HashMap;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Represents the unique identifier for an API key.
pub type ApiKeyId = i64;

/// The maximum allowed length of an API key.
pub const MAX_KEY_LEN: usize = 256;

/// The number of failed attempts before locking out.
pub const LOCKOUT_FAILURE_THRESHOLD: usize = 5;

/// The window of time in which failed attempts are tracked.
pub const LOCKOUT_WINDOW: Duration = Duration::from_secs(60);

/// The duration of a lockout.
pub const LOCKOUT_DURATION: Duration = Duration::from_secs(300);

/// The maximum number of distinct identifiers the tracker holds at once.
const MAX_TRACKED_IDENTIFIERS: usize = 50_000;

/// The maximum number of recent failure timestamps retained per identifier.
const MAX_FAILURES_PER_IDENTIFIER: usize = 32;

/// The outcome of an authentication attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    /// Authentication succeeded with the given API key identifier.
    Authorized(ApiKeyId),

    /// Authentication failed.
    Unauthorized,

    /// Authentication is locked out due to too many failures.
    LockedOut,
}

/// A storage trait for looking up active API keys.
pub trait KeyStore {
    /// Looks up an active API key by its identifier.
    fn lookup_active(&self, key_identifier: &str) -> Option<ApiKeyId>;
}

/// Computes the deterministic identifier for a presented key.
pub fn key_identifier(presented_key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(presented_key.as_bytes());
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for &b in bytes {
        out.push(*HEX.get((b >> 4) as usize).unwrap_or(&b'0') as char);
        out.push(*HEX.get((b & 0x0f) as usize).unwrap_or(&b'0') as char);
    }
    out
}

/// Checks if the presented key length is within allowed limits.
pub fn passes_guard(presented: &str) -> bool {
    let len = presented.len();
    (1..=MAX_KEY_LEN).contains(&len)
}

/// Calculates when the lockout should expire based on failure timestamps.
pub fn lockout_until(failures: &[Instant]) -> Option<Instant> {
    let mut sorted: Vec<Instant> = failures.to_vec();
    sorted.sort_unstable();

    let mut result: Option<Instant> = None;
    for (i, &anchor) in sorted.iter().enumerate() {
        let count = sorted
            .iter()
            .take(i.saturating_add(1))
            .filter(|t| anchor.saturating_duration_since(**t) <= LOCKOUT_WINDOW)
            .count();
        if count >= LOCKOUT_FAILURE_THRESHOLD
            && let Some(until) = anchor.checked_add(LOCKOUT_DURATION)
        {
            result = Some(result.map_or(until, |r| r.max(until)));
        }
    }
    result
}

/// Checks if the tracker is currently locked out.
pub fn is_locked_out(failures: &[Instant], now: Instant) -> bool {
    lockout_until(failures).is_some_and(|until| now < until)
}

/// Tracks failed authentication attempts to enforce lockouts.
#[derive(Debug, Default)]
pub struct FailureTracker {
    failures: HashMap<String, Vec<Instant>>,
}

impl FailureTracker {
    /// Creates a new empty failure tracker.
    pub fn new() -> Self {
        FailureTracker {
            failures: HashMap::new(),
        }
    }

    /// Returns true if the given identifier is currently locked out.
    pub fn is_locked(&self, identifier: &str, now: Instant) -> bool {
        self.failures
            .get(identifier)
            .is_some_and(|f| is_locked_out(f, now))
    }

    /// Records a failed authentication attempt.
    pub fn record_failure(&mut self, identifier: &str, now: Instant) {
        if !self.failures.contains_key(identifier) && self.failures.len() >= MAX_TRACKED_IDENTIFIERS
        {
            self.evict_stale(now);
            if self.failures.len() >= MAX_TRACKED_IDENTIFIERS {
                // The map is saturated with still-active entries. A
                // never-before-seen identifier cannot reach the lockout
                // threshold on a single failure, so dropping it is safe and
                // keeps memory bounded.
                return;
            }
        }

        let history = self.failures.entry(identifier.to_string()).or_default();
        history.push(now);

        let horizon = LOCKOUT_WINDOW.saturating_add(LOCKOUT_DURATION);
        history.retain(|t| now.saturating_duration_since(*t) <= horizon);

        let excess = history.len().saturating_sub(MAX_FAILURES_PER_IDENTIFIER);
        if excess > 0 {
            history.drain(0..excess);
        }
    }

    /// Drops entries whose failures have all aged out, keeping the map bounded.
    fn evict_stale(&mut self, now: Instant) {
        let horizon = LOCKOUT_WINDOW.saturating_add(LOCKOUT_DURATION);
        self.failures.retain(|_, history| {
            history.retain(|t| now.saturating_duration_since(*t) <= horizon);
            !history.is_empty()
        });
    }

    /// Clears the recorded failures for an identifier upon success.
    pub fn record_success(&mut self, identifier: &str) {
        self.failures.remove(identifier);
    }
}

/// Authenticates a presented key against the store.
pub fn authenticate<S: KeyStore + ?Sized>(
    presented: &str,
    store: &S,
    tracker: &mut FailureTracker,
    now: Instant,
) -> AuthOutcome {
    if !passes_guard(presented) {
        return AuthOutcome::Unauthorized;
    }

    let identifier = key_identifier(presented);
    authenticate_identified(&identifier, store, tracker, now)
}

/// Authenticates an already-identified key.
pub fn authenticate_identified<S: KeyStore + ?Sized>(
    identifier: &str,
    store: &S,
    tracker: &mut FailureTracker,
    now: Instant,
) -> AuthOutcome {
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

/// The result of an authentication check used in auditing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthResult {
    /// Successfully authorized.
    Authorized,

    /// Unauthorized.
    Unauthorized,

    /// Locked out.
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

/// Represents an audit log record for an authentication event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthAuditRecord {
    /// The type of authentication event.
    pub event_type: &'static str,

    /// The outcome result of the check.
    pub result: AuthResult,

    /// The identifier of the checked key.
    pub key_identifier: String,

    /// The timestamp of the authentication event.
    pub timestamp: DateTime<Utc>,
}

/// Builds an audit record using a plaintext presented key.
pub fn build_audit_record(
    presented: &str,
    outcome: &AuthOutcome,
    timestamp: DateTime<Utc>,
) -> AuthAuditRecord {
    build_audit_record_with_identifier(key_identifier(presented), outcome, timestamp)
}

/// Builds an audit record with a pre-computed key identifier.
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

        assert!(tracker.failures.is_empty());
    }

    #[test]
    fn five_failures_within_window_lock_out_for_300s() {
        let store = MapStore::empty();
        let mut tracker = FailureTracker::new();
        let base = Instant::now();

        for i in 0..4 {
            let now = base + Duration::from_secs(i * 10);
            assert_eq!(
                authenticate("bad", &store, &mut tracker, now),
                AuthOutcome::Unauthorized
            );
        }

        let trigger = base + Duration::from_secs(40);
        assert_eq!(
            authenticate("bad", &store, &mut tracker, trigger),
            AuthOutcome::Unauthorized
        );

        let during = trigger + Duration::from_secs(299);
        assert_eq!(
            authenticate("bad", &store, &mut tracker, during),
            AuthOutcome::LockedOut
        );

        let after = trigger + Duration::from_secs(301);
        assert!(!tracker.is_locked(&key_identifier("bad"), after));
    }

    #[test]
    fn failure_history_is_capped_per_identifier_and_still_locks() {
        let mut tracker = FailureTracker::new();
        let id = key_identifier("hammered");
        let now = Instant::now();

        // Hammer a single identifier far beyond the cap at the same instant.
        for _ in 0..1000 {
            tracker.record_failure(&id, now);
        }

        let history_len = tracker.failures.get(&id).map(Vec::len).unwrap_or(0);
        assert!(
            history_len <= MAX_FAILURES_PER_IDENTIFIER,
            "per-identifier history must stay bounded, got {history_len}"
        );
        // The lockout must still trigger despite the bounded history.
        assert!(tracker.is_locked(&id, now));
    }

    #[test]
    fn failures_outside_window_do_not_lock_out() {
        let mut times = Vec::new();
        let base = Instant::now();

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

        let expected = base + Duration::from_secs(20) + LOCKOUT_DURATION;
        assert_eq!(lockout_until(&times), Some(expected));
    }

    #[test]
    fn success_clears_failure_history() {
        let store = MapStore::with_key("good", 1);
        let mut tracker = FailureTracker::new();
        let base = Instant::now();

        let id = key_identifier("good");
        for i in 0..4 {
            tracker.record_failure(&id, base + Duration::from_secs(i));
        }

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
