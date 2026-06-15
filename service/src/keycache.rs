//! Shared, bounded cache of resolved API keys.
//!
//! Fronts the database lookup on the hot authentication path. There is no
//! time-based expiry: entries live until evicted by capacity (LRU) or
//! explicitly invalidated when a key is revoked, so revocation takes effect
//! immediately rather than after a TTL window. Backed by `moka`, so capacity
//! bounding is automatic.

use moka::future::Cache;

use crate::auth::ApiKeyId;

/// The resolved lookup value for an API key: its row id and optional custom
/// rate limit, or `None` when there is no active (non-revoked) key.
pub type CachedKey = Option<(ApiKeyId, Option<u32>)>;

/// Maximum number of distinct key identifiers retained.
const MAX_CACHED_KEYS: u64 = 10_000;

/// A cheaply-cloneable handle to the shared API-key cache.
#[derive(Clone)]
pub struct ApiKeyCache {
    inner: Cache<String, CachedKey>,
}

impl ApiKeyCache {
    /// Creates a new, empty cache bounded by capacity only (no TTL).
    pub fn new() -> Self {
        ApiKeyCache {
            inner: Cache::builder().max_capacity(MAX_CACHED_KEYS).build(),
        }
    }

    /// Returns the cached lookup for `identifier`, or `None` on a cache miss.
    pub async fn get(&self, identifier: &str) -> Option<CachedKey> {
        self.inner.get(identifier).await
    }

    /// Caches the resolved lookup for `identifier`.
    pub async fn insert(&self, identifier: String, value: CachedKey) {
        self.inner.insert(identifier, value).await;
    }

    /// Drops any cached lookup for `identifier` (e.g. after revocation), so the
    /// next request re-reads from the database.
    pub async fn invalidate(&self, identifier: &str) {
        self.inner.invalidate(identifier).await;
    }
}

impl Default for ApiKeyCache {
    fn default() -> Self {
        ApiKeyCache::new()
    }
}
