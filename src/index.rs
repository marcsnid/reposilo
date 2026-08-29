//! The index: rebuilt from disk, never authoritative
//! Any directory containing a `repo.json` is a repo; archive-tree folders
//! double as browseable categories at any nesting depth.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::types::{read_json, RepoManifest, SnapshotSidecar};

#[derive(Debug, Clone)]
pub struct SnapshotEntry {
    pub sidecar: SnapshotSidecar,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct RepoEntry {
    /// Absolute path of the repo directory in the archive tree.
    pub dir: PathBuf,
    /// Path relative to the archive root, e.g. "decompals/sm64".
    pub rel: String,
    pub manifest: RepoManifest,
    /// Newest first.
    pub branch_snapshots: Vec<SnapshotEntry>,
    /// Newest first.
    pub releases: Vec<SnapshotEntry>,
}

impl RepoEntry {
    fn load(dir: PathBuf, rel: String) -> Result<Self> {
        let manifest = read_json::<RepoManifest>(&dir.join("repo.json"))
            .with_context(|| format!("bad manifest in {rel}"))?;
        let branch_snapshots = load_snapshots_under(&dir.join("branch"));
        let releases = load_snapshots_under(&dir.join("releases"));
        Ok(Self { dir, rel, manifest, branch_snapshots, releases })
    }
}

fn load_snapshots_under(dir: &Path) -> Vec<SnapshotEntry> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return out;
    }
    collect_snapshots(dir, &mut out);
    out.sort_by(|a, b| b.sidecar.archived_at.cmp(&a.sidecar.archived_at));
    out
}

fn collect_snapshots(dir: &Path, out: &mut Vec<SnapshotEntry>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_snapshots(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Ok(sidecar) = read_json::<SnapshotSidecar>(&p) {
                out.push(SnapshotEntry { sidecar, dir: p.parent().unwrap().to_path_buf() });
            }
        }
    }
}

#[derive(Debug)]
pub struct Index {
    pub root: PathBuf,
    pub repos: Vec<RepoEntry>,
}

impl Index {
    /// Walk the archive tree and rebuild the index from disk.
    pub fn load(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("archive root does not exist: {}", root.display());
        }
        let mut repos = Vec::new();
        Self::walk(root, root, &mut repos)?;
        repos.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Self { root: root.to_path_buf(), repos })
    }

    fn walk(root: &Path, dir: &Path, out: &mut Vec<RepoEntry>) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let p = entry?.path();
            if !p.is_dir() {
                continue;
            }
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // skip our own fetch state and hidden dirs
            if fname == "shallow.git" || fname.starts_with('.') {
                continue;
            }
            if p.join("repo.json").exists() {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(&p)
                    .to_string_lossy()
                    .into_owned();
                out.push(RepoEntry::load(p, rel)?);
            } else {
                Self::walk(root, &p, out)?;
            }
        }
        Ok(())
    }

    pub fn find(&self, rel: &str) -> Option<&RepoEntry> {
        self.repos.iter().find(|r| r.rel == rel)
    }

    /// Filter by tag intersection (all tags must match, case-insensitive)
    /// and optional substring query over name/path/description/notes.
    pub fn filter<'a>(&'a self, tags: &[String], q: Option<&str>) -> Vec<&'a RepoEntry> {
        let ql = q.map(str::to_lowercase);
        self.repos
            .iter()
            .filter(|r| {
                tags.iter().all(|t| {
                    r.manifest
                        .tags
                        .iter()
                        .any(|rt| rt.eq_ignore_ascii_case(t.trim()))
                }) && ql.as_ref().is_none_or(|q| {
                    let hay = format!(
                        "{} {} {} {}",
                        r.manifest.name,
                        r.rel,
                        r.manifest.description.as_deref().unwrap_or(""),
                        r.manifest.notes.as_deref().unwrap_or("")
                    );
                    hay.to_lowercase().contains(q)
                })
            })
            .collect()
    }

    /// All tags with repo counts, alphabetically sorted.
    pub fn all_tags(&self) -> BTreeMap<String, usize> {
        let mut map = BTreeMap::new();
        for r in &self.repos {
            for t in &r.manifest.tags {
                *map.entry(t.clone()).or_insert(0) += 1;
            }
        }
        map
    }

    pub fn snapshot_count(&self) -> usize {
        self.repos
            .iter()
            .map(|r| r.branch_snapshots.len() + r.releases.len())
            .sum()
    }
}