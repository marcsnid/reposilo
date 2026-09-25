//! Plain-TOML configuration

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerCfg,
    pub archive: ArchiveCfg,
    pub scheduler: SchedulerCfg,
    pub retention: RetentionCfg,
    pub notifications: NotificationsCfg,
    pub git: GitCfg,
    pub github: GithubCfg,
    pub gitlab: ForgeTokenCfg,
    pub forgejo: ForgeTokenCfg,
    pub releases: ReleasesCfg,
    pub llm: LlmCfg,
    pub otel: OtelCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerCfg {
    pub bind: String,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self { bind: "127.0.0.1:8765".into() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArchiveCfg {
    /// Root of the archive tree
    pub root: String,
    /// Snapshot format: "zip" (default, universally unzip-able) or "tar.zst"
    /// (better compression for big repos).
    pub format: String,
    /// zstd compression level for tar.zst snapshots (1-22; default 10).
    pub zstd_level: i32,
}

impl Default for ArchiveCfg {
    fn default() -> Self {
        Self { root: ".".into(), format: "zip".into(), zstd_level: 10 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SchedulerCfg {
    /// Default-branch snapshot cadence.
    pub default_interval_days: u32,
    /// How often to poll remotes for new tags/releases.
    pub release_poll_hours: u32,
    /// Archive immediately when a repo is added.
    pub run_on_add: bool,
    /// Whether the background scheduler runs when serving.
    pub enabled: bool,
    /// How often (seconds) the scheduler loop looks for due repos.
    pub poll_every_secs: u32,
    /// Max concurrent background refreshes.
    pub max_concurrent: usize,
    /// Consecutive unavailability (days) before a remote is considered dead
    /// and the scheduler stops checking it. 0 = never declare dead.
    pub dead_after_days: u32,
}

impl Default for SchedulerCfg {
    fn default() -> Self {
        Self {
            default_interval_days: 7,
            release_poll_hours: 24,
            run_on_add: true,
            enabled: true,
            poll_every_secs: 900,
            max_concurrent: 2,
            dead_after_days: 21,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionCfg {
    /// Newest N default-branch snapshots to keep. -1 = keep all.
    pub keep_branch_snapshots: i64,
    /// Newest N releases to keep. -1 = keep all.
    pub keep_releases: i64,
    /// Per-tag overrides: the first rule whose tag matches a repo's tags
    /// overrides the global defaults (manifest overrides still win).
    pub tag_rules: Vec<TagRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct TagRule {
    pub tag: String,
    pub keep_branch_snapshots: Option<i64>,
    pub keep_releases: Option<i64>,
}


impl Default for RetentionCfg {
    fn default() -> Self {
        Self { keep_branch_snapshots: 1, keep_releases: 1, tag_rules: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsCfg {
    /// Optional webhook URL notified on new releases / dead remotes.
    pub webhook_url: Option<String>,
}

/// Optional OpenTelemetry OTLP/HTTP metrics export.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OtelCfg {
    pub enabled: bool,
    /// Collector base URL, e.g. http://localhost:4318 (the /v1/metrics path is
    /// appended automatically).
    pub endpoint: String,
    pub service_name: String,
    /// How often to push a snapshot, in seconds.
    pub interval_secs: u64,
}

impl Default for OtelCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: "http://localhost:4318".into(),
            service_name: "reposilo".into(),
            interval_secs: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GitCfg {
    /// Fetch depth. 1 = shallow (default). 0 = full mirror clone.
    pub depth: u32,
    /// Extra args passed to `git clone`.
    pub extra_args: Vec<String>,
    /// Hard timeout (seconds) for each git subprocess. A hung network must not
    /// hold a repo lock or a scheduler slot forever. Default 10 min.
    pub timeout_secs: u64,
}

impl Default for GitCfg {
    fn default() -> Self {
        Self { depth: 1, extra_args: Vec::new(), timeout_secs: 600 }
    }
}

/// Resolve a token that may use the `env:VAR` indirection, so secrets never
/// have to live in the config file. A missing `env:VAR` resolves to `None`
/// rather than sending the literal string as a credential.
pub fn resolve_token(token: Option<&str>) -> Option<String> {
    let t = token?;
    let resolved = if let Some(var) = t.strip_prefix("env:") {
        std::env::var(var).ok()?
    } else {
        t.to_string()
    };
    (!resolved.is_empty()).then_some(resolved)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GithubCfg {
    /// Token for GitHub API suggestions. Supports "env:VAR" indirection
    /// so the secret never has to live in this file.
    pub token: Option<String>,
}

impl GithubCfg {
    pub fn resolved_token(&self) -> Option<String> {
        resolve_token(self.token.as_deref())
    }
}

/// Token for one forge's API, with the same `env:VAR` indirection as GitHub.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ForgeTokenCfg {
    pub token: Option<String>,
}

impl ForgeTokenCfg {
    pub fn resolved_token(&self) -> Option<String> {
        resolve_token(self.token.as_deref())
    }
}

/// Release binary/asset downloading.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReleasesCfg {
    /// Which platform assets to keep. Each entry is a platform slug or a loose
    /// token ("linux-x64", "arm64", "win64", "darwin", "all"). Empty means
    /// no binaries are downloaded. Filters are ANDed per asset: an asset is
    /// downloaded when its filename names both an OS and an arch and that
    /// platform matches at least one entry.
    pub platforms: Vec<String>,
    /// Skip any single asset larger than this many MiB (0 = no limit).
    pub max_asset_mb: u64,
}

/// Local llama.cpp auto-tagger configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LlmCfg {
    pub enabled: bool,
    /// llama-server's OpenAI-compatible endpoint, e.g. http://127.0.0.1:8080/v1
    pub url: String,
    /// Optional model name passed through to the server.
    pub model: Option<String>,
    /// Repos per chat-completion request.
    pub batch_size: usize,
    /// Cap on tags merged per repo.
    pub max_tags: usize,
    /// Disable Qwen-style thinking mode via chat_template_kwargs
    /// (thinking models on CPU can generate thousands of hidden tokens).
    pub disable_thinking: bool,
    /// Per-request timeout in seconds.
    pub request_timeout_secs: u64,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://127.0.0.1:8080/v1".into(),
            model: None,
            batch_size: 10,
            max_tags: 5,
            disable_thinking: false,
            request_timeout_secs: 600,
        }
    }
}

impl Config {
    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let s = fs::read_to_string(path)
            .with_context(|| format!("cannot read config {} (run `reposilo init --root <path>` first)", path.display()))?;
        toml::from_str(&s).with_context(|| format!("invalid config {}", path.display()))
    }

    /// Make the archive root absolute (and canonical when it exists) so
    /// subprocess git ops that run with a different cwd resolve paths correctly.
    pub fn with_absolute_root(&mut self) {
        let mut p = PathBuf::from(&self.archive.root);
        if p.is_relative() {
            p = std::env::current_dir().unwrap_or_default().join(&p);
        }
        if let Ok(c) = p.canonicalize() {
            p = c;
        }
        self.archive.root = p.to_string_lossy().into_owned();
    }

    /// Save config to a TOML file (atomic: temp + rename).
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create config dir {}", parent.display()))?;
        }
        let s = toml::to_string_pretty(self)?;
        crate::types::write_atomic(path, s.as_bytes())?;
        Ok(())
    }
}

/// Default config path: $HOME/.config/reposilo/config.toml
pub fn default_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config/reposilo/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_cfg_roundtrips_toml() {
        let mut cfg = Config::default();
        cfg.releases.platforms = vec!["darwin-arm64".into(), "linux-x64".into()];
        cfg.releases.max_asset_mb = 250;
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.releases.platforms, cfg.releases.platforms);
        assert_eq!(back.releases.max_asset_mb, 250);
    }

    #[test]
    fn config_without_release_sections_loads_defaults() {
        let cfg: Config = toml::from_str("[archive]\nroot = \".\"\n").unwrap();
        assert!(cfg.releases.platforms.is_empty());
        assert_eq!(cfg.releases.max_asset_mb, 0);
        assert!(cfg.gitlab.token.is_none());
        assert!(cfg.forgejo.token.is_none());
    }

    #[test]
    fn env_token_indirection() {
        std::env::set_var("REPOSILO_TEST_TOKEN", "s3cret");
        let cfg: GithubCfg = GithubCfg { token: Some("env:REPOSILO_TEST_TOKEN".into()) };
        assert_eq!(cfg.resolved_token().as_deref(), Some("s3cret"));
        let missing = GithubCfg { token: Some("env:REPOSILO_TEST_TOKEN_MISSING".into()) };
        assert!(missing.resolved_token().is_none());
    }
}