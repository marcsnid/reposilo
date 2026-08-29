//! Best-effort GitHub REST API access (metadata enrichment + release notes).
//! All calls are optional: timeouts, errors and rate limits degrade silently.

use crate::config::Config;
use crate::forge::{self, ForgeKind};

pub struct RepoMeta {
    pub stars: u64,
    pub topics: Vec<String>,
    pub description: Option<String>,
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
    })
}

/// Release notes (markdown body) for a tag, from the GitHub releases API.
pub async fn github_release_body(cfg: &Config, origin: &str, tag: &str) -> Option<String> {
    let resp: serde_json::Value =
        gh_request(cfg, origin, &format!("/releases/tags/{tag}"))?.send().await.ok()?.json().await.ok()?;
    let body = resp["body"].as_str()?.to_string();
    (!body.trim().is_empty()).then_some(body)
}
