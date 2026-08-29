//! URL → forge detection and owner/repo extraction

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Forgejo,
    Generic,
}

impl ForgeKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Forgejo => "forgejo",
            Self::Generic => "generic",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ForgeInfo {
    pub kind: ForgeKind,
    pub owner: String,
    pub name: String,
}

/// Detect the forge type and extract owner/repo from a repo URL.
/// Handles https://, ssh (git@host:path), file:// and plain local paths
/// (the latter two count as Generic, which makes testing with local
/// fixture remotes trivial).
pub fn detect(url: &str) -> Result<ForgeInfo> {
    let u = url.trim().trim_end_matches(".git");

    let (host, path) = if let Some(rest) = u.strip_prefix("git@") {
        // scp-like ssh URL: git@github.com:owner/repo
        let (h, p) = rest.split_once(':').with_context(|| format!("cannot parse ssh git url: {url}"))?;
        (Some(h.to_string()), p.to_string())
    } else if let Some(rest) = u.strip_prefix("file://") {
        (None, rest.to_string())
    } else if let Some(rest) = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://")) {
        let (h, p) = rest.split_once('/').unwrap_or((rest, ""));
        (Some(h.to_string()), p.to_string())
    } else {
        // plain local path
        (None, u.to_string())
    };

    let segs: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segs.len() < 2 {
        bail!("url does not contain owner/repo path segments: {url}");
    }
    let owner = segs[segs.len() - 2].to_string();
    let name = segs[segs.len() - 1].to_string();

    let kind = match host.as_deref() {
        Some("github.com") => ForgeKind::GitHub,
        Some("gitlab.com") => ForgeKind::GitLab,
        // Codeberg is the flagship Forgejo instance.
        Some("codeberg.org") => ForgeKind::Forgejo,
        _ => ForgeKind::Generic,
    };

    Ok(ForgeInfo { kind, owner, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_https() {
        let i = detect("https://github.com/n64decomp/sm64.git").unwrap();
        assert_eq!(i.kind, ForgeKind::GitHub);
        assert_eq!(i.owner, "n64decomp");
        assert_eq!(i.name, "sm64");
    }

    #[test]
    fn github_ssh() {
        let i = detect("git@github.com:n64decomp/sm64.git").unwrap();
        assert_eq!(i.kind, ForgeKind::GitHub);
        assert_eq!(i.owner, "n64decomp");
        assert_eq!(i.name, "sm64");
    }

    #[test]
    fn gitlab_subgroup_takes_last_two() {
        let i = detect("https://gitlab.com/group/subgroup/project").unwrap();
        assert_eq!(i.kind, ForgeKind::GitLab);
        assert_eq!(i.owner, "subgroup");
        assert_eq!(i.name, "project");
    }

    #[test]
    fn codeberg_is_forgejo() {
        let i = detect("https://codeberg.org/dnkl/foot.git").unwrap();
        assert_eq!(i.kind, ForgeKind::Forgejo);
        assert_eq!(i.owner, "dnkl");
        assert_eq!(i.name, "foot");
    }

    #[test]
    fn local_file_url() {
        let i = detect("file:///tmp/fixture/remote").unwrap();
        assert_eq!(i.kind, ForgeKind::Generic);
        assert_eq!(i.owner, "fixture");
        assert_eq!(i.name, "remote");
    }

    #[test]
    fn too_short_fails() {
        assert!(detect("https://github.com/onlyowner").is_err());
    }
}