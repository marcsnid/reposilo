//! A small ETag revalidation cache for forge API responses.
//!
//! Every cached GET sends `If-None-Match`. A 304 means the body is unchanged,
//! and on GitHub a 304 does not count against the primary rate limit. Bodies
//! are persisted so a restart still revalidates instead of refetching.
//!
//! The cache is bounded (entry count and total bytes) with LRU eviction, and
//! every path is best-effort: a miss, an unsupported server, or a disabled
//! setting simply falls back to a normal request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::ratelimit::{send_with_pacing, RemoteGovernor};

/// Cap on cached URLs and on the total body bytes kept on disk.
const MAX_ENTRIES: usize = 512;
const MAX_BYTES: usize = 4 * 1024 * 1024;
/// Cap on a single response we are willing to buffer at all, before caching.
const MAX_RESPONSE_BYTES: u64 = 16 * 1024 * 1024;
/// Cache file under the archive root.
const CACHE_FILE: &str = "http-cache.json";
/// Writes are deferred by this much so a burst of API responses does not
/// rewrite the whole file on every request.
const SAVE_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    etag: String,
    body: String,
    /// Unix seconds, for LRU eviction.
    at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct CacheFile {
    entries: HashMap<String, CacheEntry>,
}

/// Borrowing view so saving does not clone the whole map.
#[derive(Serialize)]
struct CacheFileRef<'a> {
    entries: &'a HashMap<String, CacheEntry>,
}

/// Bounded, persistence-backed ETag cache. Counters are process lifetime
/// totals, surfaced on the Stats page and over OTLP.
#[derive(Debug, Default)]
pub struct HttpCache {
    path: Option<PathBuf>,
    entries: HashMap<String, CacheEntry>,
    hits: AtomicU64,
    misses: AtomicU64,
    /// When the file was last written, and whether changes are pending.
    last_save: Option<Instant>,
    dirty: bool,
}

impl HttpCache {
    /// Load the cache stored under `root` (empty when the file is missing or
    /// unreadable).
    pub fn load(root: &Path) -> Self {
        let path = root.join(CACHE_FILE);
        let file: CacheFile = crate::types::read_json(&path).unwrap_or_default();
        Self {
            path: Some(path),
            entries: file.entries,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            last_save: None,
            dirty: false,
        }
    }

    /// An in-memory cache with no backing file (used by tests and one-off
    /// tooling).
    pub fn empty() -> Self {
        Self::default()
    }

    /// The stored `(etag, body)` for a URL, if any.
    pub fn get(&self, url: &str) -> Option<(String, String)> {
        self.entries.get(url).map(|e| (e.etag.clone(), e.body.clone()))
    }

    /// Store a body and its ETag, prune, and persist (at most every
    /// `SAVE_INTERVAL`; call `flush` to force a write).
    pub fn put(&mut self, url: &str, etag: &str, body: &str) {
        self.entries.insert(
            url.to_string(),
            CacheEntry { etag: etag.to_string(), body: body.to_string(), at: unix_now() },
        );
        self.prune();
        self.dirty = true;
        if self.last_save.is_none_or(|t| t.elapsed() >= SAVE_INTERVAL) {
            self.save_now();
        }
    }

    /// Force any deferred writes to disk.
    pub fn flush(&mut self) {
        if self.dirty {
            self.save_now();
        }
    }

    /// Drop least-recently-stored entries until the count and byte bounds hold.
    fn prune(&mut self) {
        let total = |entries: &HashMap<String, CacheEntry>| {
            entries.values().map(|e| e.body.len()).sum::<usize>()
        };
        while self.entries.len() > MAX_ENTRIES || total(&self.entries) > MAX_BYTES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.at)
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
    }

    fn save_now(&mut self) {
        if let Some(path) = &self.path {
            let file = CacheFileRef { entries: &self.entries };
            let _ = crate::types::write_json(path, &file);
        }
        self.last_save = Some(Instant::now());
        self.dirty = false;
    }

    pub fn hit(&self) {
        self.hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn miss(&self) {
        self.misses.fetch_add(1, Ordering::Relaxed);
    }

    /// `(revalidations served from cache, fresh fetches)` since start.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits.load(Ordering::Relaxed), self.misses.load(Ordering::Relaxed))
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Lock the cache, recovering from a poisoned mutex instead of panicking so a
/// single bad request cannot take down every later one.
fn lock_cache(cache: &std::sync::Mutex<HttpCache>) -> std::sync::MutexGuard<'_, HttpCache> {
    cache.lock().unwrap_or_else(|e| e.into_inner())
}

/// A response body plus whether it came from the cache instead of the network.
pub struct Fetched {
    pub body: String,
    pub from_cache: bool,
}

/// GET `url` through the governor, revalidating with `If-None-Match` when a
/// cached ETag exists. Returns the body on success (from cache on 304), or
/// `None` when the request cannot be completed.
///
/// `build` must produce the fully configured request (client, auth, Accept).
pub async fn conditional_get<F>(
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<HttpCache>,
    url: &str,
    mut build: F,
) -> Option<Fetched>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let cached = if cfg.cache.conditional {
        lock_cache(cache).get(url)
    } else {
        None
    };
    let etag = cached.as_ref().map(|(e, _)| e.clone());
    let host = crate::ratelimit::host_key(url);
    let resp = send_with_pacing(gov, &cfg.remote, &host, || {
        let mut req = build();
        if let Some(e) = &etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, e);
        }
        req
    })
    .await?;

    if resp.status().as_u16() == 304 {
        let Some((_, body)) = cached else {
            // A 304 we cannot serve means the cache was lost; treat as a miss.
            return None;
        };
        lock_cache(cache).hit();
        tracing::debug!(url, "http cache revalidated (304)");
        return Some(Fetched { body, from_cache: true });
    }
    if !resp.status().is_success() {
        return None;
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_RESPONSE_BYTES {
            tracing::warn!(url, len, "forge API response too large; skipping");
            return None;
        }
    }
    let new_etag = resp
        .headers()
        .get(reqwest::header::ETAG)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = resp.text().await.ok()?;
    if cfg.cache.conditional {
        lock_cache(cache).miss();
        if let Some(etag) = new_etag {
            lock_cache(cache).put(url, &etag, &body);
        }
    }
    Some(Fetched { body, from_cache: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_and_roundtrip_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let mut cache = HttpCache::load(tmp.path());
            cache.put("https://api.example/x", "\"v1\"", "{\"a\":1}");
            cache.hit();
            cache.hit();
            cache.miss();
            assert_eq!(cache.stats(), (2, 1));
        }
        // A fresh load sees the persisted entry but resets the counters.
        let cache = HttpCache::load(tmp.path());
        assert_eq!(cache.get("https://api.example/x"), Some(("\"v1\"".into(), "{\"a\":1}".into())));
        assert_eq!(cache.stats(), (0, 0));
        assert!(cache.get("https://api.example/missing").is_none());
    }

    #[test]
    fn prunes_beyond_the_entry_cap() {
        let mut cache = HttpCache::empty();
        for i in 0..(MAX_ENTRIES + 50) {
            cache.put(&format!("https://api.example/{i}"), "\"e\"", "body");
        }
        assert!(cache.entries.len() <= MAX_ENTRIES, "cap must hold, got {}", cache.entries.len());
    }

    #[test]
    fn prunes_beyond_the_byte_cap() {
        let mut cache = HttpCache::empty();
        let big = "x".repeat(512 * 1024);
        for i in 0..16 {
            cache.put(&format!("https://api.example/{i}"), "\"e\"", &big);
        }
        let total: usize = cache.entries.values().map(|e| e.body.len()).sum();
        assert!(total <= MAX_BYTES, "byte cap must hold, got {total}");
    }

    /// A panic while the cache is locked must not make every later request
    /// panic too: `lock_cache` recovers from the poison.
    #[test]
    fn lock_cache_recovers_from_a_poisoned_mutex() {
        let cache = std::sync::Mutex::new(HttpCache::empty());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = cache.lock().unwrap();
            panic!("poison the mutex");
        }));
        assert!(cache.lock().is_err(), "the mutex should now be poisoned");
        let mut guard = lock_cache(&cache);
        guard.put("https://api.example/x", "\"e\"", "body");
        assert_eq!(guard.get("https://api.example/x"), Some(("\"e\"".into(), "body".into())));
    }

    /// Repeated writes within the save interval are deferred; `flush` forces
    /// them out.
    #[test]
    fn deferred_saves_flush_on_demand() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cache = HttpCache::load(tmp.path());
        cache.put("https://api.example/a", "\"e\"", "a"); // first write saves immediately
        cache.put("https://api.example/b", "\"e\"", "b"); // deferred by the save interval

        let reloaded = HttpCache::load(tmp.path());
        assert!(reloaded.get("https://api.example/a").is_some());
        assert!(reloaded.get("https://api.example/b").is_none(), "second write is deferred");

        cache.flush();
        let reloaded = HttpCache::load(tmp.path());
        assert!(reloaded.get("https://api.example/b").is_some(), "flush persists it");
    }

    /// A single body larger than the whole cache budget must be dropped, not
    /// loop forever trying to make room.
    #[test]
    fn a_single_oversized_body_is_dropped() {
        let mut cache = HttpCache::empty();
        let huge = "x".repeat(MAX_BYTES + 1);
        cache.put("https://api.example/huge", "\"e\"", &huge);
        assert!(cache.entries.is_empty(), "an oversized entry must be dropped");
    }

}