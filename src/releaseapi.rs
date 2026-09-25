//! Fetch and download release assets from forge APIs.
//!
//! Assets are not in git, so the release API is the only way to find them.
//! Each forge exposes them a little differently:
//!
//! * GitHub: `GET /repos/{o}/{r}/releases/tags/{tag}` -> `assets[]`
//! * GitLab: `GET /api/v4/projects/{id}/releases/{tag}` -> `assets.links[]`
//!   (the id is the URL-encoded full path, subgroups included)
//! * Forgejo/Gitea: `GET /api/v1/repos/{o}/{r}/releases/tags/{tag}` -> `assets[]`
//!
//! Lookups are best-effort and retried briefly on transient errors; downloads
//! verify the forge digest when one is published.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::forge::{self, ForgeKind};

#[derive(Debug, Clone)]
pub struct RemoteAsset {
    pub name: String,
    /// Public/direct download URL.
    pub url: String,
    /// GitHub API asset URL (used for authenticated downloads).
    pub api_url: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    /// Forge-provided digest, e.g. `sha256:<hex>` (GitHub). Verified after
    /// download when present.
    pub digest: Option<String>,
}

impl RemoteAsset {
    /// The filename to store under. GitLab links sometimes carry a human label
    /// instead of a filename, so fall back to the URL's last path segment.
    pub fn effective_name(&self) -> String {
        let looks_like_filename = |s: &str| {
            !s.is_empty()
                && !s.contains(' ')
                && s.rsplit(['/', '\\'])
                    .next()
                    .is_some_and(|last| last.contains('.'))
        };
        if looks_like_filename(&self.name) {
            return self.name.clone();
        }
        let base = self
            .url
            .split(['?', '#'])
            .next()
            .unwrap_or(&self.url)
            .rsplit('/')
            .next()
            .unwrap_or("");
        let decoded = percent_decode(base);
        if decoded.is_empty() {
            self.name.clone()
        } else {
            decoded
        }
    }

    /// The expected lowercase-hex sha256, if the forge supplied a usable one.
    fn expected_sha256(&self) -> Option<String> {
        let d = self.digest.as_deref()?;
        let hex = d
            .strip_prefix("sha256:")
            .unwrap_or(d)
            .trim()
            .to_ascii_lowercase();
        (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
    }
}

#[derive(Debug, Clone)]
pub struct RemoteRelease {
    pub tag: String,
    pub assets: Vec<RemoteAsset>,
}

/// A parsed origin: API base plus the full project path (GitLab subgroups
/// included, unlike `forge::detect` which only returns the last two segments).
#[derive(Debug, Clone)]
struct Origin {
    base: String,
    path: String,
}

fn parse_origin(url: &str) -> Option<Origin> {
    let u = url.trim().trim_end_matches(".git");
    let (scheme, rest, scp) = match u {
        _ if u.starts_with("https://") => ("https", &u["https://".len()..], false),
        _ if u.starts_with("http://") => ("http", &u["http://".len()..], false),
        // scp-style ssh URL; forge APIs are https
        _ if u.starts_with("git@") => ("https", &u["git@".len()..], true),
        _ => return None,
    };
    // scp uses `host:path`, http(s) uses `host/path`
    let (host, path) = if scp {
        rest.split_once(':')?
    } else {
        rest.split_once('/')?
    };
    let path = path.trim_start_matches('/').trim_end_matches('/').to_string();
    if host.is_empty() || path.is_empty() {
        return None;
    }
    Some(Origin { base: format!("{scheme}://{host}"), path })
}

/// Percent-encode a path segment set, keeping `/` encoded too (GitLab wants
/// the project path fully encoded: `group%2Fproject`).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Decode `%XX` escapes in a URL path segment (best-effort, lossy on bad
/// input) so `glab_1%2E119%2E0_amd64.deb` becomes a sane on-disk filename.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Short-timeout client for metadata lookups.
fn api_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("reposilo")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20))
        .build()
        .ok()
}

/// Client for large downloads: a per-read timeout so a stalled socket cannot
/// hang a scheduler slot, plus a generous overall deadline as a backstop so a
/// malicious/slow endpoint cannot hold a repo lock forever.
fn download_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("reposilo")
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(6 * 3600))
        .build()
        .context("build download http client")
}

/// Send a request, retrying transient failures (network errors, 429, 5xx) a
/// couple of times with a short backoff. `Retry-After` is honored up to a cap
/// so a background job cannot stall for minutes.
async fn send_retry<F>(mut build: F) -> Option<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    const ATTEMPTS: u32 = 3;
    let mut backoff = Duration::from_millis(500);
    for attempt in 0..ATTEMPTS {
        let resp = match build().send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(error = %e, "release API request failed");
                if attempt + 1 == ATTEMPTS {
                    return None;
                }
                tokio::time::sleep(backoff).await;
                backoff *= 2;
                continue;
            }
        };
        let status = resp.status();
        if status.is_success() {
            return Some(resp);
        }
        let retryable = status.is_server_error() || status.as_u16() == 429;
        if !retryable || attempt + 1 == ATTEMPTS {
            tracing::debug!(%status, "release API request not successful");
            return None;
        }
        let wait = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|s| Duration::from_secs(s.min(5)))
            .unwrap_or(backoff);
        tokio::time::sleep(wait).await;
        backoff *= 2;
    }
    None
}

/// Look up one release by tag and return its downloadable assets. `None` when
/// the forge is unsupported, the release is missing, or the API call fails.
pub async fn fetch_release(cfg: &Config, origin: &str, tag: &str) -> Option<RemoteRelease> {
    let info = forge::detect(origin).ok()?;
    match info.kind {
        ForgeKind::GitHub => github(cfg, &info, tag).await,
        ForgeKind::GitLab => gitlab(cfg, origin, tag).await,
        ForgeKind::Forgejo => forgejo(cfg, origin, &info, tag).await,
        ForgeKind::Generic => None,
    }
}

async fn github(cfg: &Config, info: &forge::ForgeInfo, tag: &str) -> Option<RemoteRelease> {
    use reqwest::header::ACCEPT;
    let client = api_client()?;
    let token = cfg.github.resolved_token();
    let url = format!(
        "https://api.github.com/repos/{}/{}/releases/tags/{}",
        info.owner,
        info.name,
        percent_encode(tag)
    );
    let resp = send_retry(|| {
        let mut req = client.get(&url).header(ACCEPT, "application/vnd.github+json");
        if let Some(tok) = token.as_deref() {
            req = req.bearer_auth(tok);
        }
        req
    })
    .await?;
    let v: Value = resp.json().await.ok()?;
    let assets = v["assets"]
        .as_array()?
        .iter()
        .filter_map(|a| {
            let name = a["name"].as_str()?.to_string();
            let api = a["url"].as_str().map(str::to_string);
            let url = a["browser_download_url"]
                .as_str()
                .map(str::to_string)
                .or_else(|| api.clone())?;
            Some(RemoteAsset {
                name,
                url,
                api_url: api,
                size: a["size"].as_u64(),
                content_type: a["content_type"].as_str().map(str::to_string),
                digest: a["digest"].as_str().map(str::to_string),
            })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

async fn gitlab(cfg: &Config, origin: &str, tag: &str) -> Option<RemoteRelease> {
    let origin = parse_origin(origin)?;
    let client = api_client()?;
    let token = cfg.gitlab.resolved_token();
    let url = format!(
        "{}/api/v4/projects/{}/releases/{}",
        origin.base,
        percent_encode(&origin.path),
        percent_encode(tag)
    );
    let resp = send_retry(|| {
        let mut req = client.get(&url);
        if let Some(tok) = token.as_deref() {
            req = req.header("PRIVATE-TOKEN", tok);
        }
        req
    })
    .await?;
    let v: Value = resp.json().await.ok()?;
    let assets = v["assets"]["links"]
        .as_array()?
        .iter()
        .filter_map(|a| {
            let name = a["name"].as_str()?.to_string();
            // direct_asset_url is the stable download link; url may be any
            // external host the maintainer attached.
            let url = a["direct_asset_url"]
                .as_str()
                .or_else(|| a["url"].as_str())?
                .to_string();
            Some(RemoteAsset { name, url, api_url: None, size: None, content_type: None, digest: None })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

async fn forgejo(cfg: &Config, origin: &str, info: &forge::ForgeInfo, tag: &str) -> Option<RemoteRelease> {
    let origin = parse_origin(origin)?;
    let client = api_client()?;
    let token = cfg.forgejo.resolved_token();
    let url = format!(
        "{}/api/v1/repos/{}/{}/releases/tags/{}",
        origin.base,
        info.owner,
        info.name,
        percent_encode(tag)
    );
    let resp = send_retry(|| {
        let mut req = client.get(&url);
        if let Some(tok) = token.as_deref() {
            req = req.header(reqwest::header::AUTHORIZATION, format!("token {tok}"));
        }
        req
    })
    .await?;
    let v: Value = resp.json().await.ok()?;
    let assets = v["assets"]
        .as_array()?
        .iter()
        .filter_map(|a| {
            let name = a["name"].as_str()?.to_string();
            let url = a["browser_download_url"].as_str()?.to_string();
            Some(RemoteAsset {
                name,
                url,
                api_url: None,
                size: a["size"].as_u64(),
                content_type: None,
                digest: None,
            })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

/// Whether a failed attempt is worth retrying. Once bytes are streaming we
/// stop retrying, so a large file is not re-downloaded on every hiccup.
enum DownloadError {
    Transient(anyhow::Error),
    Permanent(anyhow::Error),
}

/// Stream one asset to `dest` atomically, returning `(bytes, sha256)`. The
/// partial file is written to a unique `.part` and removed on failure; the
/// forge sha256 is checked before the rename.
pub async fn download_asset(
    cfg: &Config,
    origin: &str,
    asset: &RemoteAsset,
    dest: &Path,
    max_bytes: u64,
) -> Result<(u64, String)> {
    let client = download_client()?;
    let kind = forge::detect(origin).map(|i| i.kind).unwrap_or(ForgeKind::Generic);
    const ATTEMPTS: u32 = 3;
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..ATTEMPTS {
        match try_download(&client, cfg, kind, asset, dest, max_bytes).await {
            Ok(v) => return Ok(v),
            Err(DownloadError::Permanent(e)) => return Err(e),
            Err(DownloadError::Transient(e)) => {
                tracing::debug!(attempt, asset = %asset.name, error = %format!("{e:#}"), "download attempt failed");
                last = Some(e);
                if attempt + 1 < ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("download failed")))
}

async fn try_download(
    client: &reqwest::Client,
    cfg: &Config,
    kind: ForgeKind,
    asset: &RemoteAsset,
    dest: &Path,
    max_bytes: u64,
) -> Result<(u64, String), DownloadError> {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
    use tokio::io::AsyncWriteExt;

    let transient = |e: anyhow::Error| DownloadError::Transient(e);
    let permanent = |e: anyhow::Error| DownloadError::Permanent(e);

    let mut req = client.get(&asset.url);
    match kind {
        ForgeKind::GitLab => {
            if let Some(tok) = cfg.gitlab.resolved_token() {
                req = req.header("PRIVATE-TOKEN", tok);
            }
        }
        ForgeKind::Forgejo => {
            if let Some(tok) = cfg.forgejo.resolved_token() {
                req = req.header(reqwest::header::AUTHORIZATION, format!("token {tok}"));
            }
        }
        ForgeKind::GitHub | ForgeKind::Generic => {}
    }
    let mut resp = req
        .send()
        .await
        .map_err(|e| transient(e.into()))?;

    // GitHub: the public browser URL is preferred (it redirects to the CDN and
    // does not consume API rate limit). For private repos it 404s, so fall back
    // to the asset API with a token and an octet-stream Accept header.
    if !resp.status().is_success() && kind == ForgeKind::GitHub {
        if let (Some(api), Some(tok)) = (&asset.api_url, cfg.github.resolved_token()) {
            let mut headers = HeaderMap::new();
            headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
            resp = client
                .get(api)
                .headers(headers)
                .bearer_auth(tok)
                .send()
                .await
                .map_err(|e| transient(e.into()))?;
        }
    }

    let status = resp.status();
    if !status.is_success() {
        let e = anyhow::anyhow!("download failed: HTTP {status}");
        return Err(if status.is_server_error() || status.as_u16() == 429 {
            transient(e)
        } else {
            permanent(e)
        });
    }
    if let Some(len) = resp.content_length() {
        if max_bytes > 0 && len > max_bytes {
            return Err(permanent(anyhow::anyhow!(
                "asset is {len} bytes, over the {max_bytes} byte limit"
            )));
        }
    }

    // unique temp name (keep the full original name so foo.tar.gz and
    // foo.tar.bz2 cannot collide on foo.tar.part)
    let mut tmp_name = dest.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".part");
    let tmp = dest.with_file_name(tmp_name);
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| permanent(e.into()))?;
    }

    let streamed = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        while let Some(chunk) = resp.chunk().await? {
            total += chunk.len() as u64;
            if max_bytes > 0 && total > max_bytes {
                bail!("asset exceeded the {max_bytes} byte limit mid-download");
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok::<_, anyhow::Error>((total, hex::encode(hasher.finalize())))
    }
    .await;
    let (bytes, sha256) = match streamed {
        Ok(v) => v,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(permanent(e));
        }
    };

    if let Some(expected) = asset.expected_sha256() {
        if !sha256.eq_ignore_ascii_case(&expected) {
            let _ = tokio::fs::remove_file(&tmp).await;
            // integrity failure, not a transient network blip: do not spend
            // bandwidth re-downloading; the next scheduled refresh retries.
            return Err(permanent(anyhow::anyhow!(
                "sha256 mismatch for {}: expected {expected}, got {sha256}",
                asset.name
            )));
        }
    }

    tokio::fs::rename(&tmp, dest)
        .await
        .map_err(|e| permanent(anyhow::Error::new(e).context("cannot move download into place")))?;
    Ok((bytes, sha256))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_origin_keeps_gitlab_subgroups() {
        let o = parse_origin("https://gitlab.com/group/subgroup/project.git").unwrap();
        assert_eq!(o.base, "https://gitlab.com");
        assert_eq!(o.path, "group/subgroup/project");
    }

    #[test]
    fn parse_origin_handles_https_and_ssh() {
        assert_eq!(parse_origin("https://github.com/owner/repo").unwrap().path, "owner/repo");
        let o = parse_origin("git@codeberg.org:dnkl/foot.git").unwrap();
        assert_eq!(o.base, "https://codeberg.org");
        assert_eq!(o.path, "dnkl/foot");
        // http is preserved (self-hosted instances)
        assert_eq!(parse_origin("http://git.internal/team/app").unwrap().base, "http://git.internal");
        // a host with a port
        assert_eq!(
            parse_origin("http://git.internal:8080/team/app").unwrap().base,
            "http://git.internal:8080"
        );
    }

    #[test]
    fn parse_origin_rejects_non_forge_urls() {
        assert!(parse_origin("file:///tmp/fixture/remote").is_none());
        assert!(parse_origin("/tmp/local/path").is_none());
        assert!(parse_origin("https://github.com").is_none());
    }

    #[test]
    fn percent_encode_encodes_slashes_and_keeps_safe_chars() {
        assert_eq!(percent_encode("group/subgroup/project"), "group%2Fsubgroup%2Fproject");
        assert_eq!(percent_encode("v1.2.3"), "v1.2.3");
        assert_eq!(percent_encode("tag with space"), "tag%20with%20space");
    }

    #[test]
    fn percent_decode_roundtrips_and_tolerates_bad_input() {
        assert_eq!(percent_decode("glab_1%2E119%2E0_amd64.deb"), "glab_1.119.0_amd64.deb");
        assert_eq!(percent_decode("plain-name.tar.gz"), "plain-name.tar.gz");
        assert_eq!(percent_decode("bad%2"), "bad%2");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
    }

    fn asset(name: &str, url: &str) -> RemoteAsset {
        RemoteAsset {
            name: name.to_string(),
            url: url.to_string(),
            api_url: None,
            size: None,
            content_type: None,
            digest: None,
        }
    }

    #[test]
    fn effective_name_prefers_a_real_filename() {
        let a = asset(
            "glab_1.119.0_linux_amd64.tar.gz",
            "https://gitlab.com/x/-/releases/v1/downloads/glab_1.119.0_linux_amd64.tar.gz",
        );
        assert_eq!(a.effective_name(), "glab_1.119.0_linux_amd64.tar.gz");
    }

    #[test]
    fn effective_name_falls_back_to_url_for_gitlab_labels() {
        // GitLab links sometimes carry a human label rather than a filename
        let a = asset(
            "Linux binary",
            "https://gitlab.com/x/-/releases/v1/downloads/app_1%2E2_amd64.deb?foo=1",
        );
        assert_eq!(a.effective_name(), "app_1.2_amd64.deb");
    }

    /// Serve a fixed body once over a real TCP socket and run the full
    /// download path against it (no forge, Generic kind).
    async fn serve_once(body: Vec<u8>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.flush().await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn download_streams_and_verifies_digest() {
        let body = b"hello world".to_vec();
        let base = serve_once(body.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("f-linux-x64.bin");
        let sha = hex::encode(Sha256::digest(&body));
        let mut a = asset("f-linux-x64.bin", &format!("{base}/f.bin"));
        a.size = Some(body.len() as u64);
        a.digest = Some(format!("sha256:{sha}"));
        let cfg = Config::default();
        let (n, got) = download_asset(&cfg, &format!("{base}/o/r"), &a, &dest, 0)
            .await
            .unwrap();
        assert_eq!(n, body.len() as u64);
        assert_eq!(got, sha);
        assert_eq!(std::fs::read(&dest).unwrap(), body);
        // no leftover .part file
        assert!(!dest.with_file_name("f-linux-x64.bin.part").exists());
    }

    #[tokio::test]
    async fn download_rejects_a_digest_mismatch_and_leaves_nothing() {
        let body = b"hello world".to_vec();
        let base = serve_once(body.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("f-linux-x64.bin");
        let mut a = asset("f-linux-x64.bin", &format!("{base}/f.bin"));
        a.digest = Some(format!("sha256:{}", "0".repeat(64)));
        let cfg = Config::default();
        let err = download_asset(&cfg, &format!("{base}/o/r"), &a, &dest, 0)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("sha256 mismatch"));
        assert!(!dest.exists());
        assert!(!dest.with_file_name("f-linux-x64.bin.part").exists());
    }

    #[test]
    fn digest_is_validated_before_use() {
        let mut a = asset("x", "https://example/x");
        a.digest = Some("sha256:ABCDEF".into());
        assert!(a.expected_sha256().is_none(), "short digest must be ignored");
        a.digest = Some(format!("sha256:{}", "a".repeat(64)));
        assert_eq!(a.expected_sha256().unwrap(), "a".repeat(64));
        a.digest = Some("not-a-digest".into());
        assert!(a.expected_sha256().is_none());
    }
}
