//! The archive engine: shallow clones, zip snapshots, sidecars, retention
//! pruning and refreshes All git operations go through the
//! `git` CLI as a subprocess.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description as fmt_desc;
use time::OffsetDateTime;

use crate::config::Config;
use crate::forge::{self};
use crate::types::{RepoManifest, SnapshotSidecar, ZipInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    Branch,
    Release,
}

impl SnapshotKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Branch => "branch-snapshot",
            Self::Release => "release",
        }
    }
}

/// Result of refreshing a repo against its remote.
#[derive(Debug, Default, Serialize)]
pub struct RefreshSummary {
    pub repo: String,
    pub new_branch_snapshot: bool,
    pub new_release: Option<String>,
    pub pruned: Vec<String>,
    /// True when the remote could not be reached at all.
    pub remote_unavailable: bool,
}

/// Make a string safe to use as a single filesystem path component.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Best-effort changelog for a release: GitHub release notes, or the
/// matching section of CHANGELOG.md at the tag.
async fn fetch_changelog(cfg: &Config, origin: &str, git_dir: &Path, tag: &str, version: Option<&str>) -> Option<String> {
    // 1. forge release notes (GitHub only for now)
    if let Some(body) = crate::forgeapi::github_release_body(cfg, origin, tag).await {
        return Some(body);
    }
    // 2. CHANGELOG.md in the tree at the tag
    let commit = crate::files::rev_parse(git_dir, tag).await.ok()?;
    for fname in ["CHANGELOG.md", "changelog.md", "HISTORY.md", "CHANGES.md"] {
        if let Ok(bytes) = crate::files::read_file(git_dir, &commit, fname).await {
            let text = String::from_utf8_lossy(&bytes[..bytes.len().min(256 * 1024)]).into_owned();
            if let Some(section) =
                crate::files::extract_changelog_section(&text, tag, version.unwrap_or(tag))
            {
                return Some(section);
            }
        }
    }
    None
}

/// An existing snapshot file in this repo with the same commit (and format),
/// for hard-link dedup. The same commit always yields the same archive content,
/// so the two snapshots can share one inode even though their embedded metadata
/// differs. Never the file we just created.
fn find_same_commit(repo_dir: &Path, commit: &str, format: &str, just_created: &Path) -> Option<PathBuf> {
    let want = format.to_string();
    for dir in [repo_dir.join("branch"), repo_dir.join("releases")] {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for sub in entries.flatten() {
            let sub = sub.path();
            if !sub.is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(&sub) else { continue };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let Ok(sc) = crate::types::read_json::<SnapshotSidecar>(&p) else { continue };
                if sc.commit == commit && sc.format.as_deref().unwrap_or("zip") == want {
                    // The sidecar records the exact archive file name, which
                    // may itself contain dots ("proj.tar.zst"). Join that name
                    // instead of guessing with `with_extension`, which would
                    // mangle tar.zst paths.
                    let existing = p.parent().map(|d| d.join(&sc.zip.file));
                    if let Some(existing) = existing {
                        if existing != just_created && existing.exists() {
                            return Some(existing);
                        }
                    }
                }
            }
        }
    }
    None
}

/// Days elapsed since an RFC3339 timestamp (0 on parse failure).
pub fn days_since(rfc3339: &str) -> Option<f64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(rfc3339, &Rfc3339).ok()?;
    let d = time::OffsetDateTime::now_utc() - t;
    Some(d.whole_nanoseconds() as f64 / 86_400_000_000_000.0)
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Run a git subprocess with a hard timeout. `kill_on_drop` ensures a hung
/// child is killed when the timeout fires, so a black-holed network can never
/// hold a repo lock or a scheduler slot forever.
async fn git(timeout_secs: u64, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.kill_on_drop(true);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // never hang waiting for credentials in a non-interactive process
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "/usr/bin/true").env("SSH_ASKPASS", "/usr/bin/true");
    cmd.args(args);
    let out = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs.max(1)),
        cmd.output(),
    )
    .await
    {
        Ok(res) => res
            .with_context(|| format!("failed to spawn git {args:?} (is git installed?)"))?,
        Err(_) => bail!("git {args:?} timed out after {timeout_secs}s (network hang?)"),
    };
    if !out.status.success() {
        bail!(
            "git {:?} failed in {}: {}",
            args,
            cwd.map(|d| d.display().to_string()).unwrap_or_else(|| "cwd".into()),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(crate) fn sha256_file(path: &Path) -> Result<(u64, String)> {
    let mut f = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hex::encode(hasher.finalize())))
}

/// Pick the highest semver-looking tag ("v" prefix tolerated).
pub fn latest_semver_tag(tags: &[String]) -> Option<String> {
    tags.iter()
        .filter_map(|t| {
            let v = t.trim_start_matches(['v', 'V']);
            semver::Version::parse(v).ok().map(|v| (v, t.clone()))
        })
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, t)| t)
}

pub struct Archiver {
    pub cfg: Config,
}

impl Archiver {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    /// Add a repo: shallow-bare clone, manifest, initial snapshots.
    ///
    /// Fails if the repo is already archived (same URL or same owner-repo
    /// slug, wherever it lives). On any failure the partially-created repo
    /// directory is removed, so a retry starts clean instead of tripping over
    /// an orphaned `repo.json`.
    pub async fn add_repo(&self, url: &str, tags: &[String], notes: Option<String>) -> Result<PathBuf> {
        let info = forge::detect(url)?;
        let root = Path::new(&self.cfg.archive.root);
        // flat "owner-repo" folder: fork collisions (same name, different
        // owners) live side by side instead of under owner folders
        let owner = sanitize(&info.owner);
        let name = sanitize(&info.name);
        let slug = format!("{owner}-{name}");
        let repo_dir = root.join(&slug);
        if repo_dir.join("repo.json").exists() {
            bail!("already archived: {} (use `refresh`)", repo_dir.display());
        }
        // The same owner-repo slug may already live under a category folder, or
        // have been added under a different URL spelling that resolves to the
        // same slug. Refuse duplicates with a cheap directory-name scan (no
        // sidecar parsing), so `add` stays cheap on a large archive.
        if let Some(existing) = crate::index::find_repo_by_slug(root, &slug) {
            let rel = existing.strip_prefix(root).unwrap_or(&existing).to_string_lossy();
            bail!("already archived as {rel} (use `refresh`)");
        }

        // Only auto-delete on failure if this add created the directory. If it
        // pre-existed (e.g. an unregistered orphan from `delete?files=false`),
        // leave the on-disk snapshots alone rather than destroying user data.
        let pre_existing = repo_dir.exists();
        match self
            .add_repo_inner(url, &info, &repo_dir, &slug, tags, notes)
            .await
        {
            Ok(()) => Ok(repo_dir),
            Err(e) => {
                if !pre_existing {
                    // never leave a half-written repo that blocks a clean retry
                    let _ = fs::remove_dir_all(&repo_dir);
                }
                Err(e)
            }
        }
    }

    async fn add_repo_inner(
        &self,
        url: &str,
        info: &forge::ForgeInfo,
        repo_dir: &Path,
        rel: &str,
        tags: &[String],
        notes: Option<String>,
    ) -> Result<()> {
        fs::create_dir_all(repo_dir)
            .with_context(|| format!("cannot create {}", repo_dir.display()))?;
        let manifest_path = repo_dir.join("repo.json");

        // The git store is ephemeral for shallow mode (depth > 0): clone to a
        // temp dir, take the zips + metadata, drop the temp. Only full-mirror
        // mode (depth = 0) keeps the git store on disk.
        let shallow = repo_dir.join("shallow.git");
        let temp = if self.cfg.git.depth == 0 {
            self.clone_shallow(url, &shallow).await?;
            None
        } else {
            let t = tempfile::tempdir().context("tempdir for clone")?;
            self.clone_shallow(url, t.path()).await?;
            Some(t)
        };
        let git_dir = temp.as_ref().map(|t| t.path()).unwrap_or(&shallow);

        let branch = git(self.cfg.git.timeout_secs, &["symbolic-ref", "--short", "HEAD"], Some(git_dir))
            .await
            .context("could not detect default branch")?;

        // best-effort: derive a description from the README's first paragraph
        let mut description = crate::files::readme_description(git_dir, &branch).await;
        // primary language from the tree (byte-weighted extensions)
        let language = crate::files::ls_tree(git_dir, &branch)
            .await
            .ok()
            .and_then(|tree| crate::files::detect_language(&tree));
        // forge enrichment (GitHub: stars, topics → suggested tags, description)
        let (stars, suggested_tags) = match crate::forgeapi::github_repo_meta(&self.cfg, url).await {
            Some(meta) => {
                if description.is_none() {
                    description = meta.description;
                }
                (Some(meta.stars), meta.topics)
            }
            None => (None, Vec::new()),
        };

        let manifest = RepoManifest {
            origin: Some(url.to_string()),
            forge: info.kind.id().to_string(),
            name: info.name.clone(),
            added: now_rfc3339(),
            tags: tags.to_vec(),
            description,
            language,
            stars,
            suggested_tags,
            default_branch: branch.clone(),
            schedule: Default::default(),
            retention: Default::default(),
            last_checked: Some(now_rfc3339()),
            notes,
            remote_state: None,
            unavailable_since: None,
            unidentified: false,
        };
        crate::types::write_json(&manifest_path, &manifest)?;

        if self.cfg.scheduler.run_on_add {
            self.snapshot_ref(repo_dir, git_dir, rel, &info.name, url, &branch, SnapshotKind::Branch, None, None)
                .await
                .context("failed to snapshot default branch")?;

            if let Some(tag) = self.latest_release_tag(url).await? {
                match self.fetch_tag(git_dir, url, &tag).await {
                    Ok(_) => {
                        let version = tag.trim_start_matches(['v', 'V']);
                        self.snapshot_ref(repo_dir, git_dir, rel, &info.name, url, &tag, SnapshotKind::Release, Some(version), None)
                            .await
                            .context("failed to archive release")?;
                    }
                    Err(e) => {
                        tracing::warn!(repo = %rel, tag = %tag, error = %e, "could not fetch release tag");
                    }
                }
            }
            let pruned = self.prune_repo(repo_dir).await?;
            if !pruned.is_empty() {
                tracing::info!(repo = %rel, ?pruned, "pruned on add");
            }
        }
        drop(temp); // ephemeral git store goes away; the zips are the artifact
        Ok(())
    }

    async fn clone_shallow(&self, url: &str, dest: &Path) -> Result<()> {
        let depth = self.cfg.git.depth.to_string();
        let mut args: Vec<&str> = vec!["clone", "--bare"];
        if self.cfg.git.depth > 0 {
            args.push("--depth");
            args.push(&depth);
        }
        let extras: Vec<String> = self.cfg.git.extra_args.clone();
        let extra_refs: Vec<&str> = extras.iter().map(String::as_str).collect();
        args.extend(extra_refs);
        let dest_str = dest.to_string_lossy().into_owned();
        args.push(url);
        args.push(&dest_str);
        git(self.cfg.git.timeout_secs, &args, None)
            .await
            .with_context(|| format!("clone of {url} failed"))?;
        Ok(())
    }

    /// Fetch a tag shallowly into the local repo; returns its commit sha.
    async fn fetch_tag(&self, shallow: &Path, _url: &str, tag: &str) -> Result<String> {
        let depth = self.cfg.git.depth.to_string();
        let refspec = format!("refs/tags/{tag}:refs/tags/{tag}");
        let mut args: Vec<&str> = vec!["fetch"];
        if self.cfg.git.depth > 0 {
            args.push("--depth");
            args.push(&depth);
        }
        args.push("origin");
        args.push(&refspec);
        git(self.cfg.git.timeout_secs, &args, Some(shallow))
            .await
            .with_context(|| format!("fetch of tag {tag} failed"))?;
        let ref_name = format!("refs/tags/{tag}");
        git(self.cfg.git.timeout_secs, &["rev-parse", &ref_name], Some(shallow)).await
    }

    /// List all tags on the remote and pick the newest semver one.
    async fn latest_release_tag(&self, url: &str) -> Result<Option<String>> {
        let out = git(self.cfg.git.timeout_secs, &["ls-remote", "--tags", url], None)
            .await
            .with_context(|| format!("ls-remote of {url} failed"))?;
        let mut tags: Vec<String> = Vec::new();
        for line in out.lines() {
            if let Some((_, r)) = line.split_once('\t') {
                if r.ends_with("^}") {
                    continue; // peeled line for annotated tags
                }
                if let Some(name) = r.strip_prefix("refs/tags/") {
                    tags.push(name.to_string());
                }
            }
        }
        Ok(latest_semver_tag(&tags))
    }

    /// Snapshot a ref into a zip + JSON sidecar
    #[allow(clippy::too_many_arguments)]
    pub async fn snapshot_ref(
        &self,
        repo_dir: &Path,
        git_dir: &Path,
        repo_rel: &str,
        repo_name: &str,
        origin: &str,
        label: &str, // branch or tag name, used for filenames
        kind: SnapshotKind,
        version: Option<&str>,
        rev: Option<&str>, // commit-ish to archive; defaults to `label`
    ) -> Result<SnapshotSidecar> {
        let shallow = git_dir.to_path_buf();
        let rev = rev.unwrap_or(label);
        let commit = git(self.cfg.git.timeout_secs, &["rev-parse", rev], Some(&shallow))
            .await
            .with_context(|| format!("rev-parse {rev} failed"))?;
        let sha7 = &commit[..commit.len().min(7)];

        let now = OffsetDateTime::now_utc();
        let project = sanitize(repo_name);
        let use_zst = self.cfg.archive.format == "tar.zst";
        let ext = if use_zst { "tar.zst" } else { "zip" };
        let (zip_dir, base_name) = match kind {
            SnapshotKind::Branch => {
                let date = now.format(&fmt_desc!("[year]-[month]-[day]")).context("date format")?;
                // project-name-first, so a file copied out of its folder is still identifiable
                (repo_dir.join("branch").join(sanitize(label)), format!("{project}-{}@{}_{}", sanitize(label), date, sha7))
            }
            SnapshotKind::Release => {
                (repo_dir.join("releases").join(sanitize(label)), format!("{project}-{}", sanitize(label)))
            }
        };
        let zip_name = format!("{base_name}.{ext}");
        fs::create_dir_all(&zip_dir).with_context(|| format!("cannot create {}", zip_dir.display()))?;

        let zip_path = zip_dir.join(&zip_name);
        let committed_at = git(self.cfg.git.timeout_secs, &["log", "-1", "--format=%cI", rev], Some(&shallow)).await.ok();

        // changelog: forge release notes first, then CHANGELOG.md at the tag
        let changelog = if matches!(kind, SnapshotKind::Release) {
            fetch_changelog(&self.cfg, origin, git_dir, label, version)
                .await
                .map(|c| c.chars().take(32 * 1024).collect::<String>())
        } else {
            None
        };

        // Sidecar is built first with a placeholder zip block; a copy is
        // embedded INSIDE the archive (.reposilo.json) so every file is
        // self-describing when copied anywhere. The embedded copy omits the
        // self-referential zip block (it cannot know its own hash).
        let sidecar = SnapshotSidecar {
            kind: kind.as_str().to_string(),
            repo: repo_rel.to_string(),
            origin: origin.to_string(),
            r#ref: label.to_string(),
            version: version.map(str::to_string),
            commit: commit.clone(),
            committed_at,
            archived_at: now_rfc3339(),
            archiver_version: env!("CARGO_PKG_VERSION").to_string(),
            imported: None,
            imported_from: None,
            format: Some(self.cfg.archive.format.clone()),
            changelog: changelog.clone(),
            zip: ZipInfo { file: zip_name.clone(), bytes: 0, sha256: String::new() },
        };
        let mut embedded = serde_json::to_value(&sidecar).context("serialize embedded metadata")?;
        embedded
            .as_object_mut()
            .context("embedded metadata is an object")?
            .remove("zip");

        let tmp = tempfile::tempdir().context("tempdir for embedded metadata")?;
        let meta_path = tmp.path().join(".reposilo.json");
        fs::write(&meta_path, serde_json::to_vec_pretty(&embedded)?)?;

        let prefix = format!("--prefix={}/", project);
        let add_file = format!("--add-file={}", meta_path.display());
        if use_zst {
            // tar.zst: git archive to a temp tar, then stream-compress with zstd
            // A NamedTempFile in the same dir auto-deletes on drop, so a failed
            // compression (or a killed process between archive and zstd) can't
            // leave a stray .tmp.tar behind.
            let raw_tar = tempfile::NamedTempFile::new_in(&zip_dir)
                .context("create temp tar")?;
            let output = format!("--output={}", raw_tar.path().display());
            git(self.cfg.git.timeout_secs, &["archive", "--format=tar", &prefix, &add_file, &output, rev], Some(&shallow))
                .await
                .with_context(|| format!("git archive {rev} failed"))?;
            let zin = std::fs::File::open(raw_tar.path()).context("open temp tar")?;
            let mut zout = std::fs::File::create(&zip_path).context("create tar.zst")?;
            zstd::stream::copy_encode(zin, &mut zout, self.cfg.archive.zstd_level)
                .context("zstd compress")?;
            drop(zout);
            drop(raw_tar); // removes the temp tar
        } else {
            let output = format!("--output={}", zip_path.display());
            git(self.cfg.git.timeout_secs, &["archive", "--format=zip", &prefix, &add_file, &output, rev], Some(&shallow))
                .await
                .with_context(|| format!("git archive {rev} failed"))?;
        }
        drop(tmp); // clean up the temp metadata file

        let mut sidecar = sidecar;

        // content dedup: the same commit always produces the same archive
        // content, so hard-link instead of storing a second near-identical copy
        // (release tags often point at the branch HEAD we already archived).
        // The shared inode keeps the original's embedded .reposilo.json; each
        // snapshot's external sidecar remains the authority for its own
        // kind/ref/version.
        if let Some(existing) =
            find_same_commit(repo_dir, &sidecar.commit, &self.cfg.archive.format, &zip_path)
        {
            let _ = fs::remove_file(&zip_path);
            fs::hard_link(&existing, &zip_path).context("hard link dedup")?;
        }

        // hash the file as it now exists on disk (the original's bytes if linked)
        let (bytes, sha256) = sha256_file(&zip_path)?;
        sidecar.zip = ZipInfo { file: zip_name.clone(), bytes, sha256 };

        let json_path = zip_path.with_extension("json");
        crate::types::write_json(&json_path, &sidecar)?;

        // keep a plain README.md next to the metadata (< few KB): the README
        // of the default branch, browsable without opening the archive
        if matches!(kind, SnapshotKind::Branch) {
            if let Some((_, text, _)) = crate::files::readme_from_archive(&zip_path) {
                let _ = fs::write(repo_dir.join("README.md"), text);
            }
        }
        Ok(sidecar)
    }

    /// Refresh one repo against its remote: re-snapshot the default branch if
    /// it moved, archive a newer semver release if one exists, prune per
    /// retention, and record the check time
    pub async fn refresh_repo(&self, repo_dir: &Path) -> Result<RefreshSummary> {
        let manifest_path = repo_dir.join("repo.json");
        let mut manifest: RepoManifest = crate::types::read_json(&manifest_path)
            .with_context(|| format!("cannot read manifest in {}", repo_dir.display()))?;
        let shallow = repo_dir.join("shallow.git");
        let rel = repo_dir
            .strip_prefix(&self.cfg.archive.root)
            .unwrap_or(repo_dir)
            .to_string_lossy()
            .into_owned();
        let mut summary = RefreshSummary { repo: rel.clone(), ..Default::default() };

        let origin = manifest.origin.clone().unwrap_or_default();
        if origin.is_empty() {
            tracing::warn!(repo = %rel, "no origin (unidentified import); nothing to refresh");
            summary.remote_unavailable = true;
            return Ok(summary);
        }
        let name = manifest.name.clone();
        let mirror = shallow.is_dir(); // full-mirror mode keeps a persistent git store

        // Probe the remote first (cheap). If it's gone entirely, record that
        // state on the manifest and keep the local archive untouched.
        let head_out = git(self.cfg.git.timeout_secs, &["ls-remote", "--symref", &origin, "HEAD"], None).await;
        let Ok(head_out) = head_out else {
            summary.remote_unavailable = true;
            // track consecutive unavailability; after dead_after_days, declare
            // the remote dead so the scheduler stops reaching out for good
            let since = manifest.unavailable_since.clone().unwrap_or_else(now_rfc3339);
            let days = days_since(&since).unwrap_or(0.0);
            let dead = self.cfg.scheduler.dead_after_days > 0
                && days >= f64::from(self.cfg.scheduler.dead_after_days);
            let state = if dead { "dead" } else { "unavailable" };
            tracing::warn!(repo = %rel, days_since_unavailable = days, dead, "remote unreachable; keeping local copy");
            manifest.remote_state = Some(state.into());
            manifest.unavailable_since = Some(since);
            manifest.last_checked = Some(now_rfc3339());
            crate::types::write_json(&manifest_path, &manifest)?;
            return Ok(summary);
        };
        if manifest.remote_state.is_some() {
            // remote is back: resurrect and clear the unavailability tracker
            tracing::info!(repo = %rel, "remote reachable again");
            manifest.remote_state = None;
            manifest.unavailable_since = None;
        }

        // remote HEAD symref gives us the real default branch (used to heal
        // imported manifests whose default_branch is "unknown")
        let head_lines: Vec<&str> = head_out.lines().collect();
        if let Some(rb) = head_lines.first().and_then(|l| l.strip_prefix("ref: ")) {
            let rb = rb.split('\t').next().unwrap_or(rb);
            let rb = rb.strip_prefix("refs/heads/").unwrap_or(rb);
            if manifest.default_branch == "unknown" && !rb.is_empty() {
                tracing::info!(repo = %rel, branch = %rb, "detected default branch of imported repo");
                manifest.default_branch = rb.to_string();
            }
        }
        let branch = manifest.default_branch.clone();
        let branch_ref = format!("refs/heads/{branch}");

        // current archived state: compare against snapshot sidecars
        // (mirror mode can also use the local git store, but sidecars are
        // the truth in both modes; keep one code path)
        let local_sha = latest_branch_commit(repo_dir, &branch).unwrap_or_default();

        let remote_branch_sha = git(self.cfg.git.timeout_secs, &["ls-remote", &origin, &branch_ref], None)
            .await
            .ok()
            .and_then(|s| s.split('\t').next().map(str::to_string))
            .unwrap_or_default();
        let branch_changed =
            !remote_branch_sha.is_empty() && remote_branch_sha != local_sha;

        // releases check (before deciding to clone anything)
        let new_release_tag = self.latest_release_tag(&origin).await.ok().flatten();
        let release_needed = if let Some(tag) = &new_release_tag {
            let new_version = semver::Version::parse(tag.trim_start_matches(['v', 'V'])).ok();
            let old_version = archived_latest_release(repo_dir);
            match (new_version, old_version) {
                (Some(n), Some(o)) => n > o,
                (Some(_), None) => true,
                _ => false,
            }
        } else {
            false
        };

        // nothing to do? no clone at all: refresh costs one ls-remote
        let need_work = branch_changed
            || release_needed
            || manifest.language.is_none()
            || (manifest.default_branch != "unknown" && remote_branch_sha.is_empty() && !local_sha.is_empty());
        if !need_work {
            manifest.last_checked = Some(now_rfc3339());
            crate::types::write_json(&manifest_path, &manifest)?;
            return Ok(summary);
        }

        // acquire a git session: mirror = the persistent store; otherwise an
        // ephemeral depth-1 clone (shallow fetch provides no incremental
        // benefit anyway: a changed tree must be re-downloaded regardless)
        let temp = if mirror {
            if branch_changed {
                self.fetch_branch(&shallow, &origin, &branch).await?;
            }
            None
        } else {
            let t = tempfile::tempdir().context("tempdir for refresh")?;
            self.clone_shallow(&origin, t.path()).await?;
            Some(t)
        };
        let git_dir = temp.as_ref().map(|t| t.path()).unwrap_or(&shallow);

        // fill in language for repos that never had it detected (imports)
        if manifest.language.is_none() {
            if let Ok(tree) = crate::files::ls_tree(git_dir, &branch).await {
                manifest.language = crate::files::detect_language(&tree);
            }
        }

        // branch snapshot
        if branch_changed {
            self.snapshot_ref(repo_dir, git_dir, &rel, &name, &origin, &branch, SnapshotKind::Branch, None, None)
                .await?;
            summary.new_branch_snapshot = true;
        }

        // release snapshot
        if release_needed {
            if let Some(tag) = new_release_tag {
                if self.fetch_tag(git_dir, &origin, &tag).await.is_ok() {
                    let version = tag.trim_start_matches(['v', 'V']);
                    self.snapshot_ref(repo_dir, git_dir, &rel, &name, &origin, &tag, SnapshotKind::Release, Some(version), None)
                        .await?;
                    summary.new_release = Some(version.to_string());
                }
            }
        }
        drop(temp); // ephemeral git store goes away

        summary.pruned = self.prune_repo(repo_dir).await?;
        manifest.last_checked = Some(now_rfc3339());
        crate::types::write_json(&manifest_path, &manifest)?;
        Ok(summary)
    }

    async fn fetch_branch(&self, shallow: &Path, origin: &str, branch: &str) -> Result<()> {
        let depth = self.cfg.git.depth.to_string();
        // '+' forces the update in case the branch was force-pushed.
        let refspec = format!("+refs/heads/{branch}:refs/heads/{branch}");
        let mut args: Vec<&str> = vec!["fetch"];
        if self.cfg.git.depth > 0 {
            args.push("--depth");
            args.push(&depth);
        }
        args.push("origin");
        args.push(&refspec);
        git(self.cfg.git.timeout_secs, &args, Some(shallow))
            .await
            .with_context(|| format!("fetch of branch {branch} from {origin} failed"))?;
        Ok(())
    }

    /// Apply retention pruning for one repo. Returns pruned file/dir names.
    async fn prune_repo(&self, repo_dir: &Path) -> Result<Vec<String>> {
        let manifest: RepoManifest = crate::types::read_json(&repo_dir.join("repo.json"))?;
        // priority: manifest override > first matching tag rule > global default
        let mut keep_branch = self.cfg.retention.keep_branch_snapshots;
        let mut keep_releases = self.cfg.retention.keep_releases;
        for rule in &self.cfg.retention.tag_rules {
            if manifest.tags.iter().any(|t| t.eq_ignore_ascii_case(&rule.tag)) {
                if let Some(k) = rule.keep_branch_snapshots {
                    keep_branch = k;
                }
                if let Some(k) = rule.keep_releases {
                    keep_releases = k;
                }
                tracing::debug!(repo = %manifest.name, rule = %rule.tag, "per-tag retention applied");
                break;
            }
        }
        let keep_branch = manifest.retention.keep_branch_snapshots.unwrap_or(keep_branch);
        let keep_releases = manifest.retention.keep_releases.unwrap_or(keep_releases);
        let mut pruned = self.prune_branch_snapshots(repo_dir, &manifest.default_branch, keep_branch)?;
        pruned.extend(self.prune_releases(repo_dir, keep_releases)?);
        Ok(pruned)
    }

    fn prune_branch_snapshots(&self, repo_dir: &Path, branch: &str, keep: i64) -> Result<Vec<String>> {
        if keep < 0 {
            return Ok(Vec::new()); // keep all
        }
        let dir = repo_dir.join("branch").join(sanitize(branch));
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut snaps: Vec<SnapshotSidecar> = Vec::new();
        for e in entries {
            let p = e?.path();
            if p.extension().and_then(|x| x.to_str()) == Some("json") {
                if let Ok(sc) = crate::types::read_json::<SnapshotSidecar>(&p) {
                    snaps.push(sc);
                }
            }
        }
        snaps.sort_by(|a, b| b.archived_at.cmp(&a.archived_at));
        let mut pruned = Vec::new();
        for sc in snaps.iter().skip(keep as usize) {
            let zip = dir.join(&sc.zip.file);
            let json = zip.with_extension("json");
            let _ = fs::remove_file(&zip);
            let _ = fs::remove_file(&json);
            pruned.push(sc.zip.file.clone());
        }
        Ok(pruned)
    }

    fn prune_releases(&self, repo_dir: &Path, keep: i64) -> Result<Vec<String>> {
        if keep < 0 {
            return Ok(Vec::new()); // keep all
        }
        let dir = repo_dir.join("releases");
        let Ok(entries) = fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        // (version, archived_at, label, subdir)
        let mut found: Vec<(semver::Version, String, String, PathBuf)> = Vec::new();
        for e in entries {
            let p = e?.path();
            if !p.is_dir() {
                continue;
            }
            let Some(sc) = find_sidecar(&p) else { continue };
            let version = sc
                .version
                .as_deref()
                .and_then(|s| semver::Version::parse(s).ok())
                .unwrap_or(semver::Version::new(0, 0, 0));
            let label = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            found.push((version, sc.archived_at, label, p));
        }
        found.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let mut pruned = Vec::new();
        for (_, _, label, dir) in found.iter().skip(keep as usize) {
            let _ = fs::remove_dir_all(dir);
            pruned.push(label.clone());
        }
        Ok(pruned)
    }
}

fn find_sidecar(dir: &Path) -> Option<SnapshotSidecar> {
    let entries = fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Ok(sc) = crate::types::read_json::<SnapshotSidecar>(&p) {
                return Some(sc);
            }
        }
    }
    None
}

/// The newest semver version archived under releases/, if any.
/// Newest archived commit of a branch, straight from the snapshot sidecars.
fn latest_branch_commit(repo_dir: &Path, branch: &str) -> Option<String> {
    let dir = repo_dir.join("branch").join(sanitize(branch));
    let entries = fs::read_dir(&dir).ok()?;
    let mut best: Option<SnapshotSidecar> = None;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            if let Ok(sc) = crate::types::read_json::<SnapshotSidecar>(&p) {
                if sc.kind == "branch-snapshot"
                    && best.as_ref().is_none_or(|b| sc.archived_at > b.archived_at)
                {
                    best = Some(sc);
                }
            }
        }
    }
    best.map(|sc| sc.commit)
}

fn archived_latest_release(repo_dir: &Path) -> Option<semver::Version> {
    let dir = repo_dir.join("releases");
    let entries = fs::read_dir(dir).ok()?;
    let mut best: Option<semver::Version> = None;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(sc) = find_sidecar(&p) {
            if let Some(v) = sc.version.as_deref().and_then(|s| semver::Version::parse(s).ok()) {
                if best.as_ref().is_none_or(|b| v > *b) {
                    best = Some(v);
                }
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_semver_prefers_highest_and_tolerates_v() {
        let tags = vec!["0.9.0".into(), "v1.2.3".into(), "v1.10.0".into(), "nightly".into()];
        assert_eq!(latest_semver_tag(&tags).as_deref(), Some("v1.10.0"));
    }

    #[test]
    fn latest_semver_ignores_garbage() {
        let tags = vec!["latest".into(), "main".into()];
        assert_eq!(latest_semver_tag(&tags), None);
    }

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize("feature/abc-def"), "feature_abc-def");
        assert_eq!(sanitize("sm64"), "sm64");
    }
}