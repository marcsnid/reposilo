//! Plain-file metadata types: repo manifests and snapshot sidecars

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Per-repo schedule override.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScheduleCfg {
    pub interval_days: u32,
}

impl Default for ScheduleCfg {
    fn default() -> Self {
        Self { interval_days: 7 }
    }
}

/// Per-repo retention overrides; `None` = inherit from global config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RetentionOverride {
    pub keep_branch_snapshots: Option<i64>,
    pub keep_releases: Option<i64>,
}

/// A folder manifest, stored as `folder.json` in any archive-tree folder.
/// Folders without one still exist implicitly (derived from repo paths);
/// the manifest carries display settings (icon) and makes an empty folder
/// visible to the index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FolderManifest {
    /// A short emoji; empty = the default colored dot.
    pub icon: Option<String>,
}

/// The repo manifest, stored as `repo.json` in each repo directory.
/// This file is the registry: no central repo list exists
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoManifest {
    /// Origin URL; None for unidentified imports (no known remote).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// "github" | "gitlab" | "forgejo" | "generic"
    pub forge: String,
    pub name: String,
    /// RFC3339 timestamp when the repo was added.
    pub added: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Primary language detected at archive time (byte-weighted extensions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    pub default_branch: String,
    #[serde(default)]
    pub schedule: ScheduleCfg,
    #[serde(default)]
    pub retention: RetentionOverride,
    #[serde(default)]
    pub last_checked: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Set to "unavailable" (transient) or "dead" (after dead_after_days of
    /// consecutive failures) when the remote could not be reached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_state: Option<String>,
    /// When the remote was first observed unreachable (RFC3339); cleared on
    /// any successful check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_since: Option<String>,
    /// Tags suggested for this repo (forge topics, LLM suggestions).
    #[serde(default)]
    pub suggested_tags: Vec<String>,
    /// GitHub stars, when known (fetched at add time).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stars: Option<u64>,
    /// True for imported zips whose origin could not be determined.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unidentified: bool,
}

/// Info about a stored zip, embedded in the sidecar.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZipInfo {
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Metadata recorded at storage time, stored alongside each zip.
/// Everything here survives even if the remote goes away.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotSidecar {
    /// "branch-snapshot" | "release"
    pub kind: String,
    /// Repo path relative to the archive root, e.g. "decompals/sm64".
    pub repo: String,
    pub origin: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    #[serde(default)]
    pub version: Option<String>,
    pub commit: String,
    #[serde(default)]
    pub committed_at: Option<String>,
    pub archived_at: String,
    #[serde(default)]
    pub archiver_version: String,
    /// True when this snapshot came from an imported zip rather than git.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported: Option<bool>,
    /// Snapshot format: "zip" or "tar.zst" (older sidecars = zip).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Release changelog (markdown) captured at archive time, from the
    /// forge's release notes or extracted from CHANGELOG.md at the tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changelog: Option<String>,
    /// Original path of an imported zip, for provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_from: Option<String>,
    pub zip: ZipInfo,
}

/// Write `contents` to `path` atomically: temp file in the same directory,
/// then rename over the target. A crash or full disk can never leave a
/// half-written manifest/sidecar that would then fail to parse.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot create temp file next to {}", path.display()))?;
    tmp.write_all(contents)?;
    tmp.flush()?;
    let _ = tmp.as_file().sync_all();
    tmp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut s = serde_json::to_string_pretty(value)?;
    s.push('\n');
    write_atomic(path, s.as_bytes())
}

pub fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let s = fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok(serde_json::from_str(&s)?)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_json_roundtrips_and_leaves_no_temp_files() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.json");
        write_json(&p, &serde_json::json!({ "a": 1 })).unwrap();
        let v: serde_json::Value = read_json(&p).unwrap();
        assert_eq!(v["a"], 1);
        // the temp file used for the atomic rename must be gone
        let leftovers: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "x.json")
            .collect();
        assert!(leftovers.is_empty(), "unexpected leftover files: {leftovers:?}");
    }

    #[test]
    fn write_json_overwrites_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("x.json");
        write_json(&p, &serde_json::json!({ "a": 1 })).unwrap();
        write_json(&p, &serde_json::json!({ "b": 2 })).unwrap();
        let v: serde_json::Value = read_json(&p).unwrap();
        assert!(v.get("a").is_none());
        assert_eq!(v["b"], 2);
    }
}
