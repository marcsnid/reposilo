//! Outbound request pacing. One shared governor spaces calls per host so a
//! burst of repo refreshes does not trip a forge's rate limiter.
//!
//! Pacing is per host rather than per forge: `api.github.com`, `gitlab.com`, a
//! self-hosted Forgejo and an asset CDN each get their own bucket, because
//! limits are enforced per service.
//!
//! Rate-limit signals come from `Retry-After`, the `X-RateLimit-*` headers
//! (GitHub and Gitea; reset is a unix timestamp) and the `RateLimit-*` headers
//! (GitLab; reset is seconds from now). A host paused longer than
//! `max_wait_secs` is skipped instead of blocking a scheduler slot.
//!
//! With `[remote] enabled = false` every method is a no-op and requests are
//! sent immediately.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::config::RemoteCfg;
use crate::telemetry::Telemetry;

/// Per-host pacing state.
#[derive(Debug, Default)]
struct HostState {
    /// Earliest instant the next request may start (spacing).
    next_allowed: Option<Instant>,
    /// Rate-limit cooldown: no request may start before this instant.
    paused_until: Option<Instant>,
}

/// A host currently paused by a rate-limit signal (for stats/UI).
#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    pub host: String,
    pub paused_for_secs: u64,
}

/// Shared, process-wide request governor.
#[derive(Default)]
pub struct RemoteGovernor {
    hosts: Mutex<HashMap<String, HostState>>,
    /// Set once by the server. CLI runs leave it unset and only log.
    telemetry: OnceLock<Telemetry>,
}

impl RemoteGovernor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the process telemetry handle (idempotent; the first one wins).
    pub fn attach(&self, telemetry: Telemetry) {
        let _ = self.telemetry.set(telemetry);
    }

    fn note_rate_limited(&self, host: &str) {
        if let Some(t) = self.telemetry.get() {
            t.remote_rate_limited(host);
        }
    }

    fn note_skipped(&self, host: &str) {
        if let Some(t) = self.telemetry.get() {
            t.remote_requests_skipped(host);
        }
    }

    /// Wait for a pacing slot on `host`, returning `false` when the host is
    /// paused longer than `cfg.max_wait_secs` (the caller should skip the
    /// request and retry on the next scheduler pass rather than block).
    ///
    /// A no-op returning `true` when pacing is disabled.
    pub async fn acquire(&self, host: &str, cfg: &RemoteCfg) -> bool {
        if !cfg.enabled {
            return true;
        }
        let interval = Duration::from_millis(cfg.min_interval_ms);
        loop {
            let now = Instant::now();
            let wait = {
                let mut hosts = self.hosts.lock().await;
                let st = hosts.entry(host.to_owned()).or_default();
                if st.paused_until.is_some_and(|t| t <= now) {
                    st.paused_until = None;
                }
                let earliest = st
                    .next_allowed
                    .unwrap_or(now)
                    .max(st.paused_until.unwrap_or(now));
                if earliest <= now {
                    st.next_allowed =
                        Some(now + interval + Duration::from_millis(jitter_ms(cfg.jitter_ms)));
                    return true;
                }
                earliest - now
            };
            if wait > Duration::from_secs(cfg.max_wait_secs) {
                self.note_skipped(host);
                return false;
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Fold a response's rate-limit signals into the host's cooldown. Safe to
    /// call on any response (success or failure) and a no-op when disabled or
    /// `respect_rate_limits` is off.
    pub async fn observe(&self, host: &str, resp: &reqwest::Response, cfg: &RemoteCfg) {
        if !cfg.enabled || !cfg.respect_rate_limits {
            return;
        }
        let status = resp.status().as_u16();
        let mut cooldown: Option<Duration> = None;

        // Explicit server guidance first; it is the most reliable signal.
        if status == 429 || status == 503 {
            cooldown = retry_after(resp.headers());
        }
        if status == 429 && cooldown.is_none() {
            cooldown = Some(Duration::from_secs(5));
        }
        // Remaining == 0 is how GitHub/GitLab/Gitea say "you are out until reset".
        if let Some(d) = remaining_zero_cooldown(resp.headers()) {
            cooldown = Some(cooldown.map_or(d, |c| c.max(d)));
        }

        let Some(d) = cooldown else { return };
        let d = d.min(Duration::from_secs(cfg.max_cooldown_secs));
        let until = Instant::now() + d;
        let mut hosts = self.hosts.lock().await;
        let st = hosts.entry(host.to_owned()).or_default();
        st.paused_until = Some(st.paused_until.map_or(until, |p| p.max(until)));
        st.next_allowed = Some(st.next_allowed.map_or(until, |n| n.max(until)));
        tracing::debug!(
            host,
            cooldown_secs = d.as_secs_f64(),
            "remote host rate-limited; pausing"
        );
        self.note_rate_limited(host);
    }

    /// Hosts with an active cooldown right now.
    pub async fn status(&self) -> Vec<HostStatus> {
        let now = Instant::now();
        self.hosts
            .lock()
            .await
            .iter()
            .filter_map(|(host, st)| {
                let until = st.paused_until?;
                let left = until.saturating_duration_since(now);
                (left > Duration::ZERO).then(|| HostStatus {
                    host: host.clone(),
                    paused_for_secs: left.as_secs(),
                })
            })
            .collect()
    }
}

/// Send a request through the governor, retrying transient failures (network
/// errors, 429, 5xx) a couple of times. Returns `None` when pacing says the
/// host is unavailable or every attempt failed.
pub async fn send_with_pacing<F>(
    gov: &RemoteGovernor,
    cfg: &RemoteCfg,
    host: &str,
    mut build: F,
) -> Option<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    const ATTEMPTS: u32 = 3;
    let mut backoff = Duration::from_millis(500);
    for attempt in 0..ATTEMPTS {
        if !gov.acquire(host, cfg).await {
            tracing::debug!(host, "host paused beyond max wait; skipping request");
            return None;
        }
        let resp = match build().send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(host, error = %e, "request failed");
                if attempt + 1 == ATTEMPTS {
                    return None;
                }
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }
        };
        gov.observe(host, &resp, cfg).await;
        let status = resp.status();
        // 304 is a valid answer to a conditional request (the caller serves
        // the cached body); it must not be swallowed as a failure.
        if status.is_success() || status.as_u16() == 304 {
            return Some(resp);
        }
        let retryable = status.is_server_error() || status.as_u16() == 429;
        if !retryable || attempt + 1 == ATTEMPTS {
            tracing::debug!(host, %status, "request not successful");
            return None;
        }
        // On 429 the governor already recorded a cooldown, which the next
        // acquire() will wait out. When pacing is off we still back off a
        // little locally so retries are not instantaneous.
        if status.as_u16() == 429 {
            if !cfg.enabled || !cfg.respect_rate_limits {
                tokio::time::sleep(retry_after(resp.headers()).unwrap_or(backoff).min(Duration::from_secs(5))).await;
            }
        } else {
            tokio::time::sleep(backoff).await;
        }
        backoff *= 2;
    }
    None
}

/// Shared client for small API and metadata requests. Reusing one client keeps
/// a single connection pool and a single place for timeouts.
pub fn api_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent("reposilo")
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(20))
            .build()
            .expect("build shared api client")
    })
}

/// The pacing key for a URL: its host (with port when present), lowercased.
/// Handles `scheme://host[:port]/...` and scp-style `[user@]host:path`
/// (a forge origin may be either). Falls back to `"unknown"`.
pub fn host_key(url: &str) -> String {
    let u = url.trim();
    // scp-style SSH URL: [user@]host:path
    if !u.contains("://") {
        if let Some((left, _)) = u.split_once(':') {
            let host = left.rsplit('@').next().unwrap_or(left);
            if !host.is_empty() && !host.contains('/') {
                return host.to_ascii_lowercase();
            }
        }
    }
    let rest = u.split_once("://").map(|(_, r)| r).unwrap_or(u);
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim();
    if host.is_empty() {
        "unknown".to_string()
    } else {
        host.to_ascii_lowercase()
    }
}

/// `Retry-After` in seconds (the HTTP-date form is ignored; these APIs send
/// seconds in practice).
fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let v = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    v.parse::<u64>().ok().map(Duration::from_secs)
}

/// Cooldown implied by a `Remaining: 0` signal, for both header families.
fn remaining_zero_cooldown(headers: &HeaderMap) -> Option<Duration> {
    // GitHub / Gitea: X-RateLimit-Reset is a unix timestamp.
    if header_is_zero(headers, "x-ratelimit-remaining") {
        if let Some(reset) = header_u64(headers, "x-ratelimit-reset") {
            return Some(until_epoch(reset));
        }
    }
    // GitLab: RateLimit-Reset is seconds from now.
    if header_is_zero(headers, "ratelimit-remaining") {
        if let Some(reset) = header_u64(headers, "ratelimit-reset") {
            return Some(Duration::from_secs(reset));
        }
    }
    None
}

fn header_is_zero(headers: &HeaderMap, name: &str) -> bool {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim() == "0")
        .unwrap_or(false)
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers.get(name)?.to_str().ok()?.trim().parse().ok()
}

/// Time from now until a unix timestamp, saturating at zero.
fn until_epoch(epoch: u64) -> Duration {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Duration::from_secs(epoch.saturating_sub(now))
}

/// Small xorshift jitter, seeded from the clock plus a sequence counter so
/// concurrent acquires do not all pick the same offset.
fn jitter_ms(max: u64) -> u64 {
    if max == 0 {
        return 0;
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ SEQ.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x % (max + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> RemoteCfg {
        RemoteCfg {
            enabled: true,
            min_interval_ms: 50,
            jitter_ms: 0,
            respect_rate_limits: true,
            max_wait_secs: 30,
            max_cooldown_secs: 900,
        }
    }

    #[test]
    fn host_key_extracts_host_and_port() {
        assert_eq!(host_key("https://api.github.com/repos/o/r"), "api.github.com");
        assert_eq!(host_key("https://gitlab.com/api/v4/x"), "gitlab.com");
        assert_eq!(host_key("http://git.internal:8080/team/app"), "git.internal:8080");
        assert_eq!(host_key("git@codeberg.org:dnkl/foot"), "codeberg.org");
        assert_eq!(host_key(""), "unknown");
        assert_eq!(host_key("not a url"), "not a url");
    }

    #[tokio::test]
    async fn spacing_serializes_the_same_host() {
        let gov = RemoteGovernor::new();
        let c = cfg();
        let start = Instant::now();
        assert!(gov.acquire("a.example", &c).await);
        assert!(gov.acquire("a.example", &c).await);
        // second call must wait ~min_interval before reserving its slot
        assert!(start.elapsed() >= Duration::from_millis(50));
    }

    #[tokio::test]
    async fn different_hosts_do_not_block_each_other() {
        let gov = RemoteGovernor::new();
        let c = cfg();
        assert!(gov.acquire("a.example", &c).await);
        let start = Instant::now();
        assert!(gov.acquire("b.example", &c).await);
        assert!(start.elapsed() < Duration::from_millis(25));
    }

    #[tokio::test]
    async fn disabled_governor_is_a_noop() {
        let gov = RemoteGovernor::new();
        let mut c = cfg();
        c.enabled = false;
        let start = Instant::now();
        assert!(gov.acquire("a.example", &c).await);
        assert!(gov.acquire("a.example", &c).await);
        assert!(start.elapsed() < Duration::from_millis(25));
    }

    #[tokio::test]
    async fn long_pause_is_skipped_not_blocked() {
        let gov = RemoteGovernor::new();
        let mut c = cfg();
        c.max_wait_secs = 1;
        // Simulate a cooldown far in the future.
        {
            let mut hosts = gov.hosts.lock().await;
            hosts.insert(
                "a.example".into(),
                HostState {
                    next_allowed: None,
                    paused_until: Some(Instant::now() + Duration::from_secs(3600)),
                },
            );
        }
        let start = Instant::now();
        assert!(!gov.acquire("a.example", &c).await);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn remaining_zero_parses_github_epoch_and_gitlab_seconds() {
        // GitHub / Gitea: unix timestamp
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "0".parse().unwrap());
        let future = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 120;
        h.insert("x-ratelimit-reset", future.to_string().parse().unwrap());
        let d = remaining_zero_cooldown(&h).unwrap();
        assert!((119..=120).contains(&d.as_secs()), "got {d:?}");

        // GitLab: seconds from now
        let mut h = HeaderMap::new();
        h.insert("ratelimit-remaining", "0".parse().unwrap());
        h.insert("ratelimit-reset", "42".parse().unwrap());
        assert_eq!(remaining_zero_cooldown(&h), Some(Duration::from_secs(42)));

        // Non-zero remaining is ignored.
        let mut h = HeaderMap::new();
        h.insert("x-ratelimit-remaining", "17".parse().unwrap());
        h.insert("x-ratelimit-reset", "9999999999".parse().unwrap());
        assert!(remaining_zero_cooldown(&h).is_none());
    }

    #[test]
    fn retry_after_parses_seconds_and_ignores_junk() {
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "7".parse().unwrap());
        assert_eq!(retry_after(&h), Some(Duration::from_secs(7)));
        let mut h = HeaderMap::new();
        h.insert(RETRY_AFTER, "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap());
        assert_eq!(retry_after(&h), None);
    }

    #[tokio::test]
    async fn telemetry_hooks_are_optional() {
        let gov = RemoteGovernor::new();
        gov.attach(Telemetry::disabled());
        let mut c = cfg();
        c.max_wait_secs = 1;
        {
            let mut hosts = gov.hosts.lock().await;
            hosts.insert(
                "a.example".into(),
                HostState {
                    next_allowed: None,
                    paused_until: Some(Instant::now() + Duration::from_secs(3600)),
                },
            );
        }
        // Must not panic when telemetry is disabled.
        assert!(!gov.acquire("a.example", &c).await);
    }

    #[tokio::test]
    async fn status_reports_active_cooldowns() {
        let gov = RemoteGovernor::new();
        assert!(gov.status().await.is_empty());
        {
            let mut hosts = gov.hosts.lock().await;
            hosts.insert(
                "a.example".into(),
                HostState {
                    next_allowed: None,
                    paused_until: Some(Instant::now() + Duration::from_secs(30)),
                },
            );
            hosts.insert(
                "expired.example".into(),
                HostState {
                    next_allowed: None,
                    paused_until: Some(Instant::now() - Duration::from_secs(1)),
                },
            );
        }
        let s = gov.status().await;
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].host, "a.example");
        assert!(s[0].paused_for_secs <= 30);
    }
}