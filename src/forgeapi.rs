//! Best-effort GitHub REST API access (metadata enrichment + release notes).
//! All calls are optional: timeouts, errors and rate limits degrade silently.

use crate::config::Config;
use crate::forge::{self, ForgeKind};
use crate::ratelimit::{host_key, send_with_pacing, RemoteGovernor};

pub struct RepoMeta {
    pub stars: u64,
    pub topics: Vec<String>,
    pub description: Option<String>,
    /// Owner avatar URL (the icon GitHub shows next to the repo).
    pub avatar_url: Option<String>,
}

/// The public GitHub API base. Unit tests point this at a local server.
pub(crate) const GITHUB_API: &str = "https://api.github.com";

/// API URL for a GitHub repo endpoint, or `None` for a non-GitHub origin.
fn gh_url(base: &str, origin: &str, suffix: &str) -> Option<String> {
    let info = forge::detect(origin).ok()?;
    if info.kind != ForgeKind::GitHub {
        return None;
    }
    Some(format!("{base}/repos/{}/{}{suffix}", info.owner, info.name))
}

/// Authenticated GitHub API GET, paced by the shared governor and revalidated
/// through the conditional cache. Returns the response body.
async fn gh_get(
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<crate::httpcache::HttpCache>,
    url: &str,
) -> Option<String> {
    use reqwest::header::ACCEPT;
    let client = crate::ratelimit::api_client()?;
    crate::httpcache::conditional_get(cfg, gov, cache, url, || {
        let mut req = client.get(url).header(ACCEPT, "application/vnd.github+json");
        if let Some(tok) = cfg.github.resolved_token() {
            req = req.bearer_auth(tok);
        }
        req
    })
    .await
    .map(|f| f.body)
}

/// Repo metadata (stars, topics, description). GitHub repos only.
pub async fn github_repo_meta(
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<crate::httpcache::HttpCache>,
    origin: &str,
) -> Option<RepoMeta> {
    github_repo_meta_at(GITHUB_API, cfg, gov, cache, origin).await
}

/// `github_repo_meta` against an explicit API base, so tests can use a local
/// server.
async fn github_repo_meta_at(
    base: &str,
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<crate::httpcache::HttpCache>,
    origin: &str,
) -> Option<RepoMeta> {
    let url = gh_url(base, origin, "")?;
    let body = gh_get(cfg, gov, cache, &url).await?;
    let resp: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(parse_repo_meta(&resp))
}

/// Pure parser for the GitHub repo payload (kept separate so it can be tested
/// without a network call).
fn parse_repo_meta(resp: &serde_json::Value) -> RepoMeta {
    RepoMeta {
        stars: resp["stargazers_count"].as_u64().unwrap_or(0),
        topics: resp["topics"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        description: resp["description"].as_str().map(String::from),
        avatar_url: resp["owner"]["avatar_url"].as_str().map(String::from),
    }
}

/// Download a small avatar image (best-effort, capped at 2 MB). GitHub
/// avatars accept a size hint, so we ask for 64px, plenty for a 20-34px UI
/// at 2x and a fraction of the full-size image.
pub async fn fetch_avatar(cfg: &Config, gov: &RemoteGovernor, url: &str) -> Option<Vec<u8>> {
    let url = if url.contains("avatars.githubusercontent.com") {
        if url.contains('?') {
            format!("{url}&s=64")
        } else {
            format!("{url}?s=64")
        }
    } else {
        url.to_string()
    };
    let client = crate::ratelimit::api_client()?;
    let host = host_key(&url);
    let resp = send_with_pacing(gov, &cfg.remote, &host, || {
        client
            .get(&url)
            .header(reqwest::header::USER_AGENT, "reposilo")
    })
    .await?;
    if !resp.status().is_success() {
        return None;
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.is_empty() || bytes.len() > 2 * 1024 * 1024 {
        return None;
    }
    Some(bytes.to_vec())
}

/// One commit in a `compare` readout.
pub struct CompareCommit {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub date: String,
}

/// What changed between two commits (GitHub compare API).
pub struct CompareInfo {
    pub total_commits: u64,
    pub files_changed: u64,
    pub html_url: String,
    pub commits: Vec<CompareCommit>,
}

/// Commit-level comparison between `base` and `head` (GitHub repos only).
/// This is what gives branch snapshots a readable changelog even though the
/// archive itself only ever keeps shallow copies.
pub async fn github_compare(
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<crate::httpcache::HttpCache>,
    origin: &str,
    base: &str,
    head: &str,
) -> Option<CompareInfo> {
    let url = gh_url(GITHUB_API, origin, &format!("/compare/{base}...{head}"))?;
    let body = gh_get(cfg, gov, cache, &url).await?;
    let resp: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(parse_compare(&resp))
}

/// Pure parser for the GitHub compare payload (kept separate so it can be
/// unit-tested without a network call).
fn parse_compare(resp: &serde_json::Value) -> CompareInfo {
    let commits: Vec<CompareCommit> = resp["commits"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| CompareCommit {
                    sha: c["sha"].as_str().unwrap_or("").chars().take(7).collect(),
                    message: c["commit"]["message"].as_str().unwrap_or("").lines().next().unwrap_or("").to_string(),
                    author: c["commit"]["author"]["name"].as_str().unwrap_or("").to_string(),
                    date: c["commit"]["author"]["date"].as_str().unwrap_or("").chars().take(10).collect(),
                })
                .collect()
        })
        .unwrap_or_default();
    let files_changed = resp["files"].as_array().map(|a| a.len() as u64).unwrap_or(0);
    CompareInfo {
        total_commits: resp["total_commits"].as_u64().unwrap_or(commits.len() as u64),
        files_changed,
        html_url: resp["html_url"].as_str().unwrap_or("").to_string(),
        commits,
    }
}

/// Release notes (markdown body) for a tag, from the GitHub releases API.
pub async fn github_release_body(
    cfg: &Config,
    gov: &RemoteGovernor,
    cache: &std::sync::Mutex<crate::httpcache::HttpCache>,
    origin: &str,
    tag: &str,
) -> Option<String> {
    let url = gh_url(GITHUB_API, origin, &format!("/releases/tags/{tag}"))?;
    let body = gh_get(cfg, gov, cache, &url).await?;
    let resp: serde_json::Value = serde_json::from_str(&body).ok()?;
    let notes = resp["body"].as_str()?.to_string();
    (!notes.trim().is_empty()).then_some(notes)
}

#[cfg(test)]
mod tests {
    use super::{parse_compare, parse_repo_meta};

    #[test]
    fn parse_repo_meta_extracts_fields_and_tolerates_missing() {
        let payload = serde_json::json!({
            "stargazers_count": 1234,
            "topics": ["cli", "rust"],
            "description": "Fast search",
            "owner": { "avatar_url": "https://avatars.example/x" }
        });
        let meta = parse_repo_meta(&payload);
        assert_eq!(meta.stars, 1234);
        assert_eq!(meta.topics, vec!["cli", "rust"]);
        assert_eq!(meta.description.as_deref(), Some("Fast search"));
        assert_eq!(meta.avatar_url.as_deref(), Some("https://avatars.example/x"));

        let empty = parse_repo_meta(&serde_json::json!({}));
        assert_eq!(empty.stars, 0);
        assert!(empty.topics.is_empty());
        assert!(empty.description.is_none());
        assert!(empty.avatar_url.is_none());
    }

    #[test]
    fn parse_compare_extracts_commit_readout() {
        let payload = serde_json::json!({
            "total_commits": 2,
            "html_url": "https://github.com/o/r/compare/a...b",
            "files": [{"filename": "a.rs"}, {"filename": "b.rs"}],
            "commits": [
                {
                    "sha": "abcdef1234567890",
                    "commit": {
                        "message": "fix the thing\n\nbody ignored",
                        "author": {"name": "Alice", "date": "2026-10-01T12:00:00Z"}
                    }
                },
                {
                    "sha": "9876543210fedcba",
                    "commit": {
                        "message": "add feature",
                        "author": {"name": "Bob", "date": "2026-10-02T12:00:00Z"}
                    }
                }
            ]
        });
        let info = parse_compare(&payload);
        assert_eq!(info.total_commits, 2);
        assert_eq!(info.files_changed, 2);
        assert!(info.html_url.contains("compare"));
        assert_eq!(info.commits.len(), 2);
        assert_eq!(info.commits[0].sha, "abcdef1");
        assert_eq!(info.commits[0].message, "fix the thing");
        assert_eq!(info.commits[1].author, "Bob");
        assert_eq!(info.commits[1].date, "2026-10-02");
    }

    #[test]
    fn parse_compare_tolerates_missing_fields() {
        let info = parse_compare(&serde_json::json!({}));
        assert_eq!(info.total_commits, 0);
        assert!(info.commits.is_empty());
    }

    /// The full request path: URL, governor, conditional cache, auth header,
    /// and parse, against a local server.
    #[tokio::test]
    async fn github_repo_meta_end_to_end() {
        let server = crate::testserver::spawn(
            200,
            r#"{"stargazers_count":7,"topics":["cli"],"description":"d","owner":{"avatar_url":"a"}}"#,
        )
        .await;
        let mut cfg = crate::config::Config::default();
        let gov = crate::ratelimit::RemoteGovernor::new();
        let cache = std::sync::Mutex::new(crate::httpcache::HttpCache::empty());

        let meta = super::github_repo_meta_at(
            &server.base,
            &cfg,
            &gov,
            &cache,
            "https://github.com/o/r",
        )
        .await
        .unwrap();
        assert_eq!(meta.stars, 7);
        assert_eq!(meta.topics, vec!["cli"]);
        assert_eq!(meta.description.as_deref(), Some("d"));
        assert_eq!(meta.avatar_url.as_deref(), Some("a"));
        assert_eq!(server.paths(), vec!["/repos/o/r"], "the real endpoint must be used");
        assert_eq!(server.count(), 1);
        assert!(
            !server.requests.lock().unwrap()[0]
                .headers
                .to_ascii_lowercase()
                .contains("authorization"),
            "no token means no Authorization header"
        );

        // A configured token is attached to the request.
        cfg.github.token = Some("secret".into());
        let cache2 = std::sync::Mutex::new(crate::httpcache::HttpCache::empty());
        let _ = super::github_repo_meta_at(&server.base, &cfg, &gov, &cache2, "https://github.com/o/r")
            .await
            .unwrap();
        assert_eq!(server.count(), 2);
        assert!(
            server.requests.lock().unwrap()[1]
                .headers
                .to_ascii_lowercase()
                .contains("authorization: bearer secret"),
            "the token must be sent as a bearer header"
        );
    }
}
