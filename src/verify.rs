//! Archive integrity verification: do the zips on disk still match the
//! sidecars that describe them?
//!
//! The disk is the source of truth, so a full verify hashes every stored
//! snapshot and compares it to the sha256 recorded at archive time. That is
//! I/O-heavy by design, so callers get progress callbacks.

use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use crate::index::{Index, RepoEntry};

#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyProgress {
    pub done: usize,
    pub total: usize,
    pub current: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct VerifyProblem {
    pub repo: String,
    pub file: String,
    /// "missing" | "size" | "sha256" | "read"
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyReport {
    pub repos: usize,
    pub snapshots: usize,
    pub ok: usize,
    pub problems: Vec<VerifyProblem>,
    pub elapsed_ms: u128,
}

impl VerifyReport {
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }

    pub fn summary(&self) -> String {
        if self.is_clean() {
            format!("verified {} repos, {} snapshots: all good", self.repos, self.snapshots)
        } else {
            format!(
                "verified {} repos, {} snapshots: {} problem(s)",
                self.repos,
                self.snapshots,
                self.problems.len()
            )
        }
    }
}

/// Progress plus the final report for one verify run (kept per job id).
#[derive(Debug, Clone, Default, Serialize)]
pub struct VerifyJob {
    pub progress: VerifyProgress,
    pub report: Option<VerifyReport>,
}

fn short(hex: &str) -> String {
    hex.chars().take(12).collect()
}

/// Hash every stored zip and compare it to its sidecar. `only` limits the run
/// to a single repo (path relative to the archive root).
pub fn verify_archive(
    root: &Path,
    only: Option<&str>,
    mut on_progress: impl FnMut(&VerifyProgress),
) -> Result<VerifyReport> {
    let started = std::time::Instant::now();
    let index = Index::load(root)?;
    let repos: Vec<&RepoEntry> = index
        .repos
        .iter()
        .filter(|r| only.is_none_or(|o| r.rel == o))
        .collect();
    if let Some(o) = only {
        if repos.is_empty() {
            anyhow::bail!("no such repo: {o}");
        }
    }

    let total: usize = repos
        .iter()
        .map(|r| r.branch_snapshots.len() + r.releases.len())
        .sum();
    let mut report = VerifyReport { repos: repos.len(), ..Default::default() };
    let mut progress = VerifyProgress { total, ..Default::default() };

    for repo in repos {
        for entry in repo.branch_snapshots.iter().chain(repo.releases.iter()) {
            report.snapshots += 1;
            progress.current = format!("{}/{}", repo.rel, entry.sidecar.zip.file);
            on_progress(&progress);

            let zip = entry.dir.join(&entry.sidecar.zip.file);
            let mut problem = |kind: &str, detail: String| {
                report.problems.push(VerifyProblem {
                    repo: repo.rel.clone(),
                    file: entry.sidecar.zip.file.clone(),
                    kind: kind.to_string(),
                    detail,
                });
            };

            match std::fs::metadata(&zip) {
                Err(_) => problem("missing", "file not found".into()),
                Ok(md) => {
                    if md.len() != entry.sidecar.zip.bytes {
                        problem(
                            "size",
                            format!("{} on disk, {} in sidecar", md.len(), entry.sidecar.zip.bytes),
                        );
                    }
                    if entry.sidecar.zip.sha256.is_empty() {
                        // very old sidecars may predate hashing; size is all we have
                        report.ok += 1;
                    } else {
                        match crate::archiver::sha256_file(&zip) {
                            Ok((_, sha)) if sha.eq_ignore_ascii_case(&entry.sidecar.zip.sha256) => {
                                report.ok += 1;
                            }
                            Ok((_, sha)) => problem(
                                "sha256",
                                format!("expected {}, got {}", short(&entry.sidecar.zip.sha256), short(&sha)),
                            ),
                            Err(e) => problem("read", format!("{e:#}")),
                        }
                    }
                }
            }

            progress.done += 1;
            on_progress(&progress);
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RepoManifest, SnapshotSidecar, ZipInfo};

    fn fixture(root: &Path) -> std::path::PathBuf {
        let repo = root.join("owner-demo");
        let rel = repo.join("releases").join("v1.0.0");
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(
            repo.join("repo.json"),
            serde_json::to_vec(&RepoManifest {
                origin: Some("https://github.com/owner/demo".into()),
                forge: "github".into(),
                name: "demo".into(),
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
                unidentified: false,
            })
            .unwrap(),
        )
        .unwrap();

        let zip = rel.join("demo-v1.0.0.zip");
        std::fs::write(&zip, b"hello").unwrap();
        let (bytes, sha256) = crate::archiver::sha256_file(&zip).unwrap();
        let sidecar = SnapshotSidecar {
            kind: "release".into(),
            repo: "owner-demo".into(),
            origin: "https://github.com/owner/demo".into(),
            r#ref: "v1.0.0".into(),
            version: Some("1.0.0".into()),
            commit: "abc".into(),
            committed_at: None,
            archived_at: "2025-01-02T00:00:00Z".into(),
            archiver_version: "test".into(),
            imported: None,
            imported_from: None,
            format: Some("zip".into()),
            changelog: None,
            assets: vec![],
            assets_filters: vec![],
            assets_max_mb: 0,
            zip: ZipInfo { file: "demo-v1.0.0.zip".into(), bytes, sha256 },
        };
        crate::types::write_json(&zip.with_extension("json"), &sidecar).unwrap();
        zip
    }

    #[test]
    fn clean_archive_verifies() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let report = verify_archive(tmp.path(), None, |_| {}).unwrap();
        assert!(report.is_clean(), "{:?}", report.problems);
        assert_eq!(report.repos, 1);
        assert_eq!(report.snapshots, 1);
        assert_eq!(report.ok, 1);
    }

    #[test]
    fn missing_and_corrupt_zips_are_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = fixture(tmp.path());
        std::fs::write(&zip, b"tampered!").unwrap();
        let report = verify_archive(tmp.path(), None, |_| {}).unwrap();
        assert!(!report.is_clean());
        assert!(report.problems.iter().any(|p| p.kind == "size"));
        assert!(report.problems.iter().any(|p| p.kind == "sha256"));
        assert_eq!(report.ok, 0);

        std::fs::remove_file(&zip).unwrap();
        let report = verify_archive(tmp.path(), None, |_| {}).unwrap();
        assert!(report.problems.iter().any(|p| p.kind == "missing"));
    }

    #[test]
    fn unknown_repo_filter_errors() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        assert!(verify_archive(tmp.path(), Some("nope"), |_| {}).is_err());
    }

    #[test]
    fn progress_reaches_total() {
        let tmp = tempfile::tempdir().unwrap();
        fixture(tmp.path());
        let mut seen = 0usize;
        verify_archive(tmp.path(), None, |p| seen = seen.max(p.done)).unwrap();
        assert_eq!(seen, 1);
    }
}
