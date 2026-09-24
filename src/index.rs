//! The index: rebuilt from disk, never authoritative
//! Any directory containing a `repo.json` is a repo; archive-tree folders
//! double as browseable categories at any nesting depth.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::types::{read_json, FolderManifest, RepoManifest, SnapshotSidecar};

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

/// A folder with an explicit `folder.json` on disk (folders also exist
/// implicitly for any path prefix of a repo, but only explicit ones can
/// be empty and/or carry an icon).
#[derive(Debug, Clone)]
pub struct FolderEntry {
    pub dir: PathBuf,
    /// Path relative to the archive root, e.g. "games/decomp".
    pub rel: String,
    pub manifest: FolderManifest,
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
    /// Folders carrying a folder.json (a superset of the explicit folders;
    /// implicit prefixes are derived from repo rels at render time).
    pub folders: Vec<FolderEntry>,
}

impl Index {
    /// Walk the archive tree and rebuild the index from disk.
    pub fn load(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("archive root does not exist: {}", root.display());
        }
        let mut repos = Vec::new();
        let mut folders = Vec::new();
        Self::walk(root, root, &mut repos, &mut folders)?;
        repos.sort_by(|a, b| a.rel.cmp(&b.rel));
        folders.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Self { root: root.to_path_buf(), repos, folders })
    }

    fn walk(root: &Path, dir: &Path, out: &mut Vec<RepoEntry>, folders: &mut Vec<FolderEntry>) -> Result<()> {
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
                // One unreadable manifest must not take down the whole archive:
                // skip it (loudly) so the server still starts and reindexes.
                match RepoEntry::load(p, rel.clone()) {
                    Ok(repo) => out.push(repo),
                    Err(e) => tracing::error!(
                        repo = %rel,
                        error = %format!("{e:#}"),
                        "skipping repo with an unreadable manifest"
                    ),
                }
            } else {
                if p.join("folder.json").exists() {
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .into_owned();
                    let manifest = read_json::<FolderManifest>(&p.join("folder.json"))
                        .unwrap_or_default();
                    folders.push(FolderEntry { dir: p.clone(), rel, manifest });
                }
                Self::walk(root, &p, out, folders)?;
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

/// Cheap duplicate check used on `add`: is there a repo directory whose folder
/// name equals `slug` anywhere in the tree? Walks directory names only (no
/// `repo.json`/sidecar parsing) and never descends into a repo's snapshot dirs,
/// so it stays cheap even for a large archive. Returns the repo dir if found.
pub fn find_repo_by_slug(root: &Path, slug: &str) -> Option<PathBuf> {
    fn walk(dir: &Path, slug: &str) -> Option<PathBuf> {
        let entries = fs::read_dir(dir).ok()?;
        for e in entries.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if fname == "shallow.git" || fname.starts_with('.') {
                continue;
            }
            if fname == slug && p.join("repo.json").exists() {
                return Some(p);
            }
            // a *registered* repo dir (any name) with a manifest: don't descend
            // into its branch/releases trees; just keep looking for our slug
            if p.join("repo.json").exists() {
                continue;
            }
            if let Some(found) = walk(&p, slug) {
                return Some(found);
            }
        }
        None
    }
    walk(root, slug)
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_repo_by_slug_walks_categories_but_not_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // a repo nested under a category folder
        let repo = root.join("category").join("owner-repo");
        fs::create_dir_all(repo.join("branch").join("master")).unwrap();
        fs::write(repo.join("repo.json"), "{}").unwrap();
        // noise: a hidden dir, an internal git store, and a decoy inside snapshots
        fs::create_dir_all(root.join(".hidden").join("owner-repo")).unwrap();
        fs::create_dir_all(root.join("category").join("shallow.git")).unwrap();
        fs::create_dir_all(repo.join("branch").join("master").join("owner-repo")).unwrap();

        assert_eq!(find_repo_by_slug(root, "owner-repo").as_deref(), Some(repo.as_path()));
        assert!(find_repo_by_slug(root, "does-not-exist").is_none());

        // an unregistered orphan dir (matching slug, no repo.json) must NOT
        // match, so the repo can be re-added after `delete?files=false`
        let orphan = root.join("orphan-repo");
        fs::create_dir_all(orphan.join("branch").join("master")).unwrap();
        assert!(find_repo_by_slug(root, "orphan-repo").is_none());
    }

    /// A single unreadable manifest must not fail the whole index (that would
    /// stop the server from starting); it is skipped with a log instead.
    #[test]
    fn load_skips_a_corrupt_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let good = root.join("owner-good");
        fs::create_dir_all(&good).unwrap();
        fs::write(
            good.join("repo.json"),
            r#"{"forge":"generic","name":"good","added":"2025-01-01T00:00:00Z","default_branch":"master"}"#,
        )
        .unwrap();
        let bad = root.join("owner-bad");
        fs::create_dir_all(&bad).unwrap();
        fs::write(bad.join("repo.json"), "{ this is not json").unwrap();

        let index = Index::load(root).unwrap();
        assert_eq!(index.repos.len(), 1);
        assert_eq!(index.repos[0].rel, "owner-good");
    }
}
