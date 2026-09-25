//! Fetch and download release assets from forge APIs.
//!
//! Binary assets are NOT stored in git, so the release notes API is the only
//! way to discover them. Each forge exposes them differently:
//!
//! * **GitHub**  `GET /repos/{o}/{r}/releases/tags/{tag}` → `assets[]`
//!   (private downloads need the asset API URL + `Accept: octet-stream`)
//! * **GitLab**  `GET /api/v4/projects/{id}/releases/{tag}` → `assets.links[]`
//!   (project id is the URL-encoded full path, subgroups included)
//! * **Forgejo/Gitea** `GET /api/v1/repos/{o}/{r}/releases/tags/{tag}` → `assets[]`
//!
//! Everything here is best-effort: a missing token, a rate limit or a forge
//! that does not expose releases simply yields no assets.

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
}

impl RemoteAsset {
    /// The filename to store this asset under. GitHub/Forgejo always report a
    /// filename, but GitLab release links may carry a human label ("Linux
    /// binary") instead; in that case fall back to the last path segment of
    /// the direct download URL.
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
}

#[derive(Debug, Clone)]
pub struct RemoteRelease {
    pub tag: String,
    pub assets: Vec<RemoteAsset>,
}

/// Split an origin URL into its API base and full project path. Keeps GitLab
/// subgroups (unlike `forge::detect`, which only returns the last two
/// segments). Returns `(base, host, path)`, e.g.
/// `("https://gitlab.com", "gitlab.com", "group/subgroup/project")`.
fn parse_origin(url: &str) -> Option<(String, String, String)> {
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
    Some((format!("{scheme}://{host}"), host.to_string(), path))
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

fn api_client(timeout_secs: u64) -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("reposilo")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .ok()
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
    let client = api_client(20)?;
    let mut req = client
        .get(format!(
            "https://api.github.com/repos/{}/{}/releases/tags/{}",
            info.owner,
            info.name,
            percent_encode(tag)
        ))
        .header(ACCEPT, "application/vnd.github+json");
    if let Some(tok) = cfg.github.resolved_token() {
        req = req.bearer_auth(tok);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), tag, "github release lookup failed");
        return None;
    }
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
            })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

async fn gitlab(cfg: &Config, origin: &str, tag: &str) -> Option<RemoteRelease> {
    let (base, _host, path) = parse_origin(origin)?;
    let client = api_client(20)?;
    let mut req = client.get(format!(
        "{base}/api/v4/projects/{}/releases/{}",
        percent_encode(&path),
        percent_encode(tag)
    ));
    if let Some(tok) = cfg.gitlab.resolved_token() {
        req = req.header("PRIVATE-TOKEN", tok);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), tag, "gitlab release lookup failed");
        return None;
    }
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
            Some(RemoteAsset { name, url, api_url: None, size: None, content_type: None })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

async fn forgejo(cfg: &Config, origin: &str, info: &forge::ForgeInfo, tag: &str) -> Option<RemoteRelease> {
    let (base, _host, _path) = parse_origin(origin)?;
    let client = api_client(20)?;
    let mut req = client.get(format!(
        "{base}/api/v1/repos/{}/{}/releases/tags/{}",
        info.owner,
        info.name,
        percent_encode(tag)
    ));
    if let Some(tok) = cfg.forgejo.resolved_token() {
        req = req.header(reqwest::header::AUTHORIZATION, format!("token {tok}"));
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        tracing::debug!(status = %resp.status(), tag, "forgejo release lookup failed");
        return None;
    }
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
            })
        })
        .collect();
    Some(RemoteRelease { tag: tag.to_string(), assets })
}

/// Stream one asset to `dest` atomically, returning `(bytes, sha256)`.
/// A partial download is written to `dest.part` and removed on any failure.
pub async fn download_asset(
    cfg: &Config,
    origin: &str,
    asset: &RemoteAsset,
    dest: &Path,
    max_bytes: u64,
) -> Result<(u64, String)> {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT};
    use tokio::io::AsyncWriteExt;

    let kind = forge::detect(origin).map(|i| i.kind).unwrap_or(ForgeKind::Generic);
    let client = api_client(6 * 3600).context("build http client")?;
    let mut req = client.get(&asset.url);
    match kind {
        ForgeKind::GitHub => {
            // Private repos: the API asset URL with an octet-stream Accept
            // header streams the file. browser_download_url is public-only.
            if let (Some(api), Some(tok)) = (&asset.api_url, cfg.github.resolved_token()) {
                let mut headers = HeaderMap::new();
                headers.insert(ACCEPT, HeaderValue::from_static("application/octet-stream"));
                req = client.get(api).headers(headers).bearer_auth(tok);
            }
        }
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
        ForgeKind::Generic => {}
    }

    let mut resp = req.send().await.with_context(|| format!("GET {}", asset.url))?;
    if !resp.status().is_success() {
        bail!("download failed: HTTP {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if max_bytes > 0 && len > max_bytes {
            bail!("asset is {len} bytes, over the {max_bytes} byte limit");
        }
    }

    let tmp = dest.with_extension("part");
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let write = async {
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

    match write {
        Ok((bytes, sha)) => {
            tokio::fs::rename(&tmp, dest)
                .await
                .with_context(|| format!("cannot move {} into place", dest.display()))?;
            Ok((bytes, sha))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_origin_keeps_gitlab_subgroups() {
        let (base, host, path) =
            parse_origin("https://gitlab.com/group/subgroup/project.git").unwrap();
        assert_eq!(base, "https://gitlab.com");
        assert_eq!(host, "gitlab.com");
        assert_eq!(path, "group/subgroup/project");
    }

    #[test]
    fn parse_origin_handles_https_and_ssh() {
        let (_, _, p) = parse_origin("https://github.com/owner/repo").unwrap();
        assert_eq!(p, "owner/repo");
        let (base, _, p) = parse_origin("git@codeberg.org:dnkl/foot.git").unwrap();
        assert_eq!(base, "https://codeberg.org");
        assert_eq!(p, "dnkl/foot");
        // http is preserved (self-hosted instances)
        let (base, _, _) = parse_origin("http://git.internal/team/app").unwrap();
        assert_eq!(base, "http://git.internal");
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
}
