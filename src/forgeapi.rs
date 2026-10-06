//! Best-effort GitHub REST API access (metadata enrichment + release notes).
//! All calls are optional: timeouts, errors and rate limits degrade silently.

use crate::config::Config;
use crate::forge::{self, ForgeKind};

pub struct RepoMeta {
    pub stars: u64,
    pub topics: Vec<String>,
    pub description: Option<String>,
    /// Owner avatar URL (the icon GitHub shows next to the repo).
    pub avatar_url: Option<String>,
}

fn gh_request(cfg: &Config, origin: &str, suffix: &str) -> Option<reqwest::RequestBuilder> {
    use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, USER_AGENT};
    let info = forge::detect(origin).ok()?;
    if info.kind != ForgeKind::GitHub {
        return None;
    }
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/vnd.github+json"));
    headers.insert(USER_AGENT, HeaderValue::from_static("reposilo"));
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let mut req = client.get(format!(
        "https://api.github.com/repos/{}/{}{suffix}",
        info.owner, info.name
    ));
    if let Some(tok) = cfg.github.resolved_token() {
        req = req.bearer_auth(tok);
    }
    Some(req)
}

/// Repo metadata (stars, topics, description). GitHub repos only.
pub async fn github_repo_meta(cfg: &Config, origin: &str) -> Option<RepoMeta> {
    let resp: serde_json::Value = gh_request(cfg, origin, "")?.send().await.ok()?.json().await.ok()?;
    Some(RepoMeta {
        stars: resp["stargazers_count"].as_u64().unwrap_or(0),
        topics: resp["topics"]
            .as_array()
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        description: resp["description"].as_str().map(String::from),
        avatar_url: resp["owner"]["avatar_url"].as_str().map(String::from),
    })
}

/// Download a small avatar image (best-effort, capped at 2 MB). GitHub
/// avatars accept a size hint, so we ask for 64px — plenty for a 20–34px UI
/// at 2x and a fraction of the full-size image.
pub async fn fetch_avatar(url: &str) -> Option<Vec<u8>> {
    let url = if url.contains("avatars.githubusercontent.com") {
        if url.contains('?') {
            format!("{url}&s=64")
        } else {
            format!("{url}?s=64")
        }
    } else {
        url.to_string()
    };
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "reposilo")
        .send()
        .await
        .ok()?;
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
    origin: &str,
    base: &str,
    head: &str,
) -> Option<CompareInfo> {
    let resp: serde_json::Value = gh_request(cfg, origin, &format!("/compare/{base}...{head}"))?
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
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
pub async fn github_release_body(cfg: &Config, origin: &str, tag: &str) -> Option<String> {
    let resp: serde_json::Value =
        gh_request(cfg, origin, &format!("/releases/tags/{tag}"))?.send().await.ok()?.json().await.ok()?;
    let body = resp["body"].as_str()?.to_string();
    (!body.trim().is_empty()).then_some(body)
}

#[cfg(test)]
mod tests {
    use super::parse_compare;

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
}
