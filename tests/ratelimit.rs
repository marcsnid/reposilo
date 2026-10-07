//! Integration tests for the outbound request governor.
//!
//! These run against a real local HTTP server instead of a mock, so the full
//! reqwest and pacing path is exercised. Coverage:
//!   * per-host spacing (same host waits, different hosts do not)
//!   * the disabled fallback sends immediately
//!   * Retry-After (429) pauses the host
//!   * GitHub and Gitea X-RateLimit-Remaining: 0 with an epoch reset
//!   * GitLab RateLimit-Remaining: 0 with a seconds-from-now reset
//!   * a cooldown longer than max_wait_secs is skipped, not blocked on
//!   * git operations (a real `git ls-remote`) are paced, and the fallback
//!     sends them back to back
//!   * a conditional GET revalidates with an ETag and serves the cached body
//!     on a 304, while the disabled setting refetches
//!
//! Assertions measure the gap between recorded request times instead of
//! wall-clock totals, so they hold up under parallel test load.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reposilo::archiver::Archiver;
use reposilo::config::{Config, RemoteCfg};
use reposilo::ratelimit::{send_with_pacing, RemoteGovernor};
use reposilo::types::RepoManifest;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Clone)]
struct Resp {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Resp {
    fn ok() -> Self {
        Resp { status: 200, headers: vec![] }
    }
    fn with(status: u16, headers: &[(&str, &str)]) -> Self {
        Resp {
            status,
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }
}

/// Start a server that serves `responses` in order (repeating the last one)
/// and records the instant of every request. The server runs on its own OS
/// thread with its own runtime, so the test future can never starve it.
async fn spawn_server(responses: Vec<Resp>) -> (String, Arc<Mutex<Vec<Instant>>>) {
    // Warm the shared client: its one-time init (~hundreds of ms) would
    // otherwise delay the first request past the spacing window and mask the
    // governor's behavior.
    let _ = client();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let hits: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let hits2 = hits.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let mut i = 0usize;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let _ = sock.set_nodelay(true);
                hits2.lock().unwrap().push(Instant::now());
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let r = responses.get(i).or_else(|| responses.last()).cloned();
                i += 1;
                let Some(r) = r else { continue };
                let body = b"{}";
                let mut out = format!(
                    "HTTP/1.1 {} OK\r\nContent-Length: {}\r\nConnection: close\r\n",
                    r.status,
                    body.len()
                )
                .into_bytes();
                for (k, v) in &r.headers {
                    out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
                }
                out.extend_from_slice(b"\r\n");
                out.extend_from_slice(body);
                // A single write avoids Nagle/delayed-ACK between header and body.
                let _ = sock.write_all(&out).await;
                let _ = sock.flush().await;
            }
        });
    });
    (format!("http://{addr}"), hits)
}

fn cfg() -> RemoteCfg {
    RemoteCfg {
        enabled: true,
        min_interval_ms: 100,
        jitter_ms: 0,
        respect_rate_limits: true,
        max_wait_secs: 5,
        max_cooldown_secs: 60,
    }
}

async fn hit(gov: &RemoteGovernor, c: &RemoteCfg, url: &str) -> Option<reqwest::Response> {
    send_with_pacing(gov, c, &reposilo::ratelimit::host_key(url), || client().get(url)).await
}

/// One shared client so per-call construction is not mistaken for pacing.
fn client() -> &'static reqwest::Client {
    static C: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    C.get_or_init(reqwest::Client::new)
}

/// Gap between the first and last recorded hit.
fn gap(hits: &Arc<Mutex<Vec<Instant>>>) -> Duration {
    let h = hits.lock().unwrap();
    assert!(h.len() >= 2, "expected at least two requests");
    h[h.len() - 1].duration_since(h[0])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pacing_spaces_requests_to_the_same_host() {
    let (base, hits) = spawn_server(vec![Resp::ok()]).await;
    let gov = RemoteGovernor::new();
    let c = cfg();
    let url = format!("{base}/x");

    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(hit(&gov, &c, &url).await.is_some());

    assert!(
        gap(&hits) >= Duration::from_millis(80),
        "same host should be spaced by ~min_interval, gap was {:?}",
        gap(&hits)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_hosts_are_independent() {
    let (a, ha) = spawn_server(vec![Resp::ok()]).await;
    let (b, hb) = spawn_server(vec![Resp::ok()]).await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.min_interval_ms = 3000;

    assert!(hit(&gov, &c, &format!("{a}/x")).await.is_some());
    assert!(hit(&gov, &c, &format!("{b}/x")).await.is_some());

    // If the two hosts shared a bucket, the second hit would be >= 3000ms later.
    let first_a = ha.lock().unwrap()[0];
    let first_b = hb.lock().unwrap()[0];
    let delta = if first_b >= first_a { first_b - first_a } else { first_a - first_b };
    assert!(
        delta < Duration::from_millis(1000),
        "a different host must not wait on the first host's spacing, gap was {delta:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_fallback_sends_immediately() {
    let (base, hits) = spawn_server(vec![Resp::ok()]).await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.enabled = false;
    c.min_interval_ms = 3000;
    let url = format!("{base}/x");

    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(
        gap(&hits) < Duration::from_millis(1000),
        "disabled pacing must not add delay, gap was {:?}",
        gap(&hits)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_pauses_the_host_then_recovers() {
    // First response is 429 with Retry-After: 1, then success.
    let (base, hits) = spawn_server(vec![
        Resp::with(429, &[("Retry-After", "1")]),
        Resp::ok(),
    ])
    .await;
    let gov = RemoteGovernor::new();
    let c = cfg();
    let url = format!("{base}/x");

    let resp = hit(&gov, &c, &url).await;
    assert!(resp.is_some(), "should recover after the cooldown");
    assert!(
        gap(&hits) >= Duration::from_millis(900),
        "Retry-After must be honored, gap was {:?}",
        gap(&hits)
    );
    assert_eq!(hits.lock().unwrap().len(), 2, "exactly one retry");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_after_beyond_max_wait_is_skipped() {
    // Always 429 with a long Retry-After; max_wait_secs is tiny.
    let (base, hits) = spawn_server(vec![Resp::with(429, &[("Retry-After", "60")])]).await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.max_wait_secs = 1;
    c.max_cooldown_secs = 60;
    let url = format!("{base}/x");

    let start = Instant::now();
    assert!(hit(&gov, &c, &url).await.is_none());
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "a long cooldown must be skipped, not waited out"
    );
    assert_eq!(hits.lock().unwrap().len(), 1, "must not hammer the host");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_ratelimit_exhaustion_pauses_even_on_success() {
    // A 200 whose X-RateLimit-Remaining is 0: the request succeeds, but the
    // host must be paused. The reset is far away and capped by max_cooldown so
    // the assertion is deterministic.
    let reset = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
    let (base, hits) = spawn_server(vec![Resp::with(
        200,
        &[
            ("X-RateLimit-Remaining", "0"),
            ("X-RateLimit-Reset", &reset.to_string()),
        ],
    )])
    .await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.max_cooldown_secs = 2;
    let url = format!("{base}/x");

    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(
        gap(&hits) >= Duration::from_millis(1800),
        "GitHub reset header must pause the next request, gap was {:?}",
        gap(&hits)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitlab_ratelimit_exhaustion_uses_seconds_reset() {
    let (base, hits) = spawn_server(vec![Resp::with(
        200,
        &[("RateLimit-Remaining", "0"), ("RateLimit-Reset", "1")],
    )])
    .await;
    let gov = RemoteGovernor::new();
    let c = cfg();
    let url = format!("{base}/x");

    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(
        gap(&hits) >= Duration::from_millis(800),
        "GitLab reset header must pause the next request, gap was {:?}",
        gap(&hits)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn respect_rate_limits_off_ignores_cooldown_headers() {
    let (base, hits) = spawn_server(vec![Resp::with(
        200,
        &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", "9999999999")],
    )])
    .await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.respect_rate_limits = false;
    // No ordinary spacing either, so the only thing that could delay the
    // second request is a rate-limit cooldown, which must be ignored.
    c.min_interval_ms = 0;
    c.max_cooldown_secs = 60;
    let url = format!("{base}/x");

    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(hit(&gov, &c, &url).await.is_some());
    assert!(
        gap(&hits) < Duration::from_millis(1000),
        "with respect_rate_limits=false only the normal spacing applies, gap was {:?}",
        gap(&hits)
    );
}

/// Build an archive with one manifest pointing at a local HTTP server, then
/// run two refreshes. Each refresh does one network git probe (`ls-remote`),
/// the server answers 500 so git stops after a single request, and we return
/// the time between the first and last request the server saw.
async fn two_git_refresh_gap(min_interval_ms: u64, enabled: bool) -> (Duration, usize) {
    let (base, hits) = spawn_server(vec![Resp::with(500, &[])]).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let repo = root.join("owner-repo");
    std::fs::create_dir_all(&repo).unwrap();

    let manifest = RepoManifest {
        origin: Some(format!("{base}/owner/repo.git")),
        forge: "generic".into(),
        name: "repo".into(),
        added: "2025-01-01T00:00:00Z".into(),
        tags: vec![],
        description: None,
        language: None,
        default_branch: "main".into(),
        schedule: Default::default(),
        retention: Default::default(),
        last_checked: None,
        notes: None,
        remote_state: None,
        unavailable_since: None,
        suggested_tags: vec![],
        stars: None,
        color: None,
        unidentified: false,
    };
    reposilo::types::write_json(&repo.join("repo.json"), &manifest).unwrap();

    let mut cfg = Config::default();
    cfg.archive.root = root.to_string_lossy().into_owned();
    cfg.remote.enabled = enabled;
    cfg.remote.min_interval_ms = min_interval_ms;
    cfg.remote.jitter_ms = 0;
    cfg.remote.max_wait_secs = 5;
    let archiver = Archiver::new(cfg);

    let _ = archiver.refresh_repo(&repo).await;
    let _ = archiver.refresh_repo(&repo).await;

    let h = hits.lock().unwrap().clone();
    let gap = if h.len() >= 2 { h[h.len() - 1].duration_since(h[0]) } else { Duration::ZERO };
    (gap, h.len())
}

/// The git call sites must actually be paced end to end, through a real git
/// subprocess talking HTTP, not just in the governor's unit tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_operations_are_paced_end_to_end() {
    let (gap, hits) = two_git_refresh_gap(800, true).await;
    assert!(hits >= 2, "expected two git probes, got {hits}");
    assert!(
        gap >= Duration::from_millis(600),
        "network git operations must be spaced, gap was {gap:?}"
    );
}

/// And the fallback must send git operations back to back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_pacing_disabled_sends_back_to_back() {
    let (gap, hits) = two_git_refresh_gap(800, false).await;
    assert!(hits >= 2, "expected two git probes, got {hits}");
    assert!(
        gap < Duration::from_millis(400),
        "disabled pacing must not delay git operations, gap was {gap:?}"
    );
}

/// A server that serves `body` with an ETag, answers 304 when the client
/// revalidates with a matching `If-None-Match`, and records per request
/// whether it carried that header.
async fn spawn_etag_server(
    body: &'static str,
    etag: &'static str,
) -> (String, Arc<Mutex<Vec<bool>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let reqs: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let reqs2 = reqs.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let _ = sock.set_nodelay(true);
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let conditional = req.contains("if-none-match");
                reqs2.lock().unwrap().push(conditional);
                let out = if conditional {
                    b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .into_bytes()
                };
                let _ = sock.write_all(&out).await;
                let _ = sock.flush().await;
            }
        });
    });
    (format!("http://{addr}"), reqs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_get_revalidates_with_etag() {
    use reposilo::httpcache::{conditional_get, HttpCache};
    let (base, reqs) = spawn_etag_server("{\"ok\":true}", "\"v1\"").await;
    let url = format!("{base}/x");
    let mut cfg = Config::default();
    cfg.remote.min_interval_ms = 0;
    cfg.remote.jitter_ms = 0;
    let gov = RemoteGovernor::new();
    let cache = std::sync::Mutex::new(HttpCache::empty());
    let client = reqwest::Client::new();

    let first = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert_eq!(first.body, "{\"ok\":true}");
    assert!(!first.from_cache, "first call must hit the network");

    let second = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert_eq!(second.body, "{\"ok\":true}");
    assert!(second.from_cache, "second call must be served from the cache");
    assert_eq!(cache.lock().unwrap().stats(), (1, 1), "one hit, one miss");

    let r = reqs.lock().unwrap();
    assert_eq!(r.len(), 2);
    assert!(!r[0], "first request must be unconditional");
    assert!(r[1], "second request must carry If-None-Match");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_get_disabled_refetches() {
    use reposilo::httpcache::{conditional_get, HttpCache};
    let (base, reqs) = spawn_etag_server("{\"ok\":true}", "\"v1\"").await;
    let url = format!("{base}/x");
    let mut cfg = Config::default();
    cfg.remote.min_interval_ms = 0;
    cfg.remote.jitter_ms = 0;
    cfg.cache.conditional = false;
    let gov = RemoteGovernor::new();
    let cache = std::sync::Mutex::new(HttpCache::empty());
    let client = reqwest::Client::new();

    let _ = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    let second = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert!(!second.from_cache, "disabled cache must refetch");
    assert!(!reqs.lock().unwrap()[1], "disabled cache must not send If-None-Match");
}
/// A 500 is retried with backoff before giving up.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retries_server_errors_then_succeeds() {
    let (base, hits) = spawn_server(vec![Resp::with(500, &[]), Resp::ok()]).await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.min_interval_ms = 0;
    c.jitter_ms = 0;
    let url = format!("{base}/x");

    let start = Instant::now();
    let resp = hit(&gov, &c, &url).await;
    assert!(resp.is_some(), "a 500 should be retried and then succeed");
    assert_eq!(hits.lock().unwrap().len(), 2, "exactly one retry");
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "backoff must be applied, elapsed {:?}",
        start.elapsed()
    );
}

/// Without an ETag there is nothing to revalidate, so every call refetches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_get_without_etag_always_refetches() {
    use reposilo::httpcache::{conditional_get, HttpCache};
    let (base, _hits) = spawn_server(vec![Resp::ok()]).await;
    let url = format!("{base}/x");
    let mut cfg = Config::default();
    cfg.remote.min_interval_ms = 0;
    cfg.remote.jitter_ms = 0;
    let gov = RemoteGovernor::new();
    let cache = std::sync::Mutex::new(HttpCache::empty());
    let client = reqwest::Client::new();

    let first = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    let second = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert!(!first.from_cache && !second.from_cache);
    assert_eq!(cache.lock().unwrap().stats(), (0, 2), "no ETag means no cache hit");
}

/// A server whose ETag and body can change between calls. Returns the base
/// URL, the shared state, and whether each request carried If-None-Match.
async fn spawn_mutable_etag_server(
) -> (String, Arc<Mutex<(String, String)>>, Arc<Mutex<Vec<bool>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let state: Arc<Mutex<(String, String)>> =
        Arc::new(Mutex::new(("\"v1\"".into(), "{\"v\":1}".into())));
    let reqs: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(Vec::new()));
    let state2 = state.clone();
    let reqs2 = reqs.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let _ = sock.set_nodelay(true);
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let conditional = req.contains("if-none-match");
                reqs2.lock().unwrap().push(conditional);
                let (etag, body) = state2.lock().unwrap().clone();
                let out = if conditional && req.contains(&etag.to_ascii_lowercase()) {
                    b"HTTP/1.1 304 Not Modified\r\nConnection: close\r\n\r\n".to_vec()
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nETag: {etag}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .into_bytes()
                };
                let _ = sock.write_all(&out).await;
                let _ = sock.flush().await;
            }
        });
    });
    (format!("http://{addr}"), state, reqs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conditional_get_updates_when_the_resource_changes() {
    use reposilo::httpcache::{conditional_get, HttpCache};
    let (base, state, reqs) = spawn_mutable_etag_server().await;
    let url = format!("{base}/x");
    let mut cfg = Config::default();
    cfg.remote.min_interval_ms = 0;
    cfg.remote.jitter_ms = 0;
    let gov = RemoteGovernor::new();
    let cache = std::sync::Mutex::new(HttpCache::empty());
    let client = reqwest::Client::new();

    let first = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert_eq!(first.body, "{\"v\":1}");
    assert!(!first.from_cache);

    // The resource changes server-side.
    *state.lock().unwrap() = ("\"v2\"".into(), "{\"v\":2}".into());

    let second = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert_eq!(second.body, "{\"v\":2}", "a changed resource must be fetched");
    assert!(!second.from_cache, "a non-matching ETag is a fresh fetch");

    // The cache now holds v2 and revalidates.
    let third = conditional_get(&cfg, &gov, &cache, &url, || client.get(&url)).await.unwrap();
    assert_eq!(third.body, "{\"v\":2}");
    assert!(third.from_cache, "the updated ETag must revalidate");

    let r = reqs.lock().unwrap();
    assert_eq!(r.len(), 3);
    assert!(!r[0], "the first request has nothing to revalidate");
    assert!(r[1] && r[2], "later requests must be conditional");
}

/// Concurrent acquires for the same host must serialize, not race.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_acquires_are_serialized() {
    let gov = Arc::new(RemoteGovernor::new());
    let mut c = cfg();
    c.min_interval_ms = 40;
    c.jitter_ms = 0;
    c.max_wait_secs = 5;

    let start = Instant::now();
    let mut handles = Vec::new();
    for _ in 0..5 {
        let gov = gov.clone();
        let c = c.clone();
        handles.push(tokio::spawn(async move { gov.acquire("a.example", &c).await }));
    }
    for h in handles {
        assert!(h.await.unwrap(), "every acquire should get a slot");
    }
    assert!(
        start.elapsed() >= Duration::from_millis(160),
        "5 slots at 40ms must span at least 160ms, was {:?}",
        start.elapsed()
    );
}

/// A server can send an absurd Retry-After; the cooldown must be capped
/// instead of overflowing the instant arithmetic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn huge_retry_after_is_capped_not_overflowed() {
    let (base, _hits) = spawn_server(vec![Resp::with(
        429,
        &[("Retry-After", "18446744073709551615")],
    )])
    .await;
    let gov = RemoteGovernor::new();
    let mut c = cfg();
    c.max_cooldown_secs = u64::MAX;
    c.max_wait_secs = 1;
    let url = format!("{base}/x");

    let start = Instant::now();
    assert!(hit(&gov, &c, &url).await.is_none(), "a huge cooldown must be skipped");
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "must not actually wait out the capped cooldown"
    );
}
