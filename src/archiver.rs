//! The archive engine: shallow clones, zip snapshots, sidecars, retention
//! pruning and refreshes All git operations go through the
//! `git` CLI as a subprocess.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::macros::format_description as fmt_desc;
use time::OffsetDateTime;

use crate::config::Config;
use crate::forge::{self};
use crate::ratelimit::RemoteGovernor;
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
    /// Commit count in the new branch snapshot, when a compare was available.
    pub branch_commits: Option<u64>,
    /// Markdown changelog for the new branch snapshot (commit readout).
    pub branch_changelog: Option<String>,
}

/// Make a string safe to use as a single filesystem path component.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Split a folder path into its tag segments: `games/tools` becomes `games`
/// and `tools`. Empty segments are dropped.
pub fn folder_tags(folder: &str) -> Vec<String> {
    folder
        .split('/')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Keep a repo's tags in step with the folder it lives in, when `[tags]
/// folders` is on. Tags matching a segment of the old folder are dropped (they
/// were location-derived) unless the new folder also uses that segment, and
/// every segment of the new folder is added. Manual tags are left alone.
pub fn apply_folder_tags(
    tags: &mut Vec<String>,
    old_folder: &str,
    new_folder: &str,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    let old = folder_tags(old_folder);
    let new = folder_tags(new_folder);
    tags.retain(|t| {
        !old.iter().any(|o| o.eq_ignore_ascii_case(t))
            || new.iter().any(|n| n.eq_ignore_ascii_case(t))
    });
    for seg in &new {
        if !tags.iter().any(|t| t.eq_ignore_ascii_case(seg)) {
            tags.push(seg.clone());
        }
    }
    tags.sort();
    tags.dedup();
}

/// Ensure every prefix of `folder` exists on disk with a `folder.json`, so a
/// folder created by adding or moving a repo persists (and stays visible) after
/// that repo later leaves. Existing manifests are left untouched.
pub fn ensure_folder_manifests(root: &Path, folder: &str) -> Result<()> {
    let folder = folder.trim().trim_matches('/');
    if folder.is_empty() {
        return Ok(());
    }
    let mut prefix = String::new();
    for part in folder.split('/') {
        if part.is_empty() {
            continue;
        }
        prefix = if prefix.is_empty() { part.to_string() } else { format!("{prefix}/{part}") };
        let dir = root.join(&prefix);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create folder {}", dir.display()))?;
        let manifest = dir.join("folder.json");
        if !manifest.exists() {
            crate::types::write_json(&manifest, &crate::types::FolderManifest::default())?;
        }
    }
    Ok(())
}

/// Remove now-empty implicit folders left behind by a move. Stops at the
/// archive root, at a folder that has a `folder.json` (an explicit folder is
/// kept even when empty), and at any folder that still holds something.
pub fn sweep_empty_folders(root: &Path, start: &Path) {
    let mut cur = start.to_path_buf();
    while cur.starts_with(root) && cur != root {
        if cur.join("folder.json").exists() {
            break;
        }
        let empty = fs::read_dir(&cur).map(|mut it| it.next().is_none()).unwrap_or(false);
        if !empty {
            break;
        }
        if fs::remove_dir(&cur).is_err() {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => break,
        }
    }
}

/// Render a branch-snapshot changelog from a GitHub compare result.
fn render_branch_changelog(info: &crate::forgeapi::CompareInfo) -> String {
    let mut out = String::new();
    out.push_str(&format!("**{} commit(s)**", info.total_commits));
    if info.files_changed > 0 {
        out.push_str(&format!(" · **{} file(s) changed**", info.files_changed));
    }
    if !info.html_url.is_empty() {
        out.push_str(&format!(" · [view on GitHub]({})", info.html_url));
    }
    out.push_str("\n\n");
    for c in &info.commits {
        out.push_str(&format!("- `{}` {} · {} ({})\n", c.sha, c.message, c.author, c.date));
    }
    out
}

/// Best-effort changelog for a release: GitHub release notes, or the
/// matching section of CHANGELOG.md at the tag.
async fn fetch_changelog(cfg: &Config, gov: &RemoteGovernor, cache: &std::sync::Mutex<crate::httpcache::HttpCache>, origin: &str, git_dir: &Path, tag: &str, version: Option<&str>) -> Option<String> {
    // 1. forge release notes (GitHub only for now)
    if let Some(body) = crate::forgeapi::github_release_body(cfg, gov, cache, origin, tag).await {
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

/// Rewrite the `repo` field of every snapshot sidecar under `repo_dir` to the
/// fully scoped archive path. Called after a repo folder is moved so the
/// on-disk metadata keeps matching the location the index reports. Writes only
/// when the field actually changed.
pub fn rewrite_sidecar_repo(repo_dir: &Path, rel: &str) -> Result<()> {
    fn walk(dir: &Path, rel: &str) -> Result<()> {
        let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // release assets live under releases/<tag>/assets and never
                // hold sidecars
                if p.file_name().and_then(|n| n.to_str()) == Some("assets")
                    && dir.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()) == Some("releases")
                {
                    continue;
                }
                walk(&p, rel)?;
            } else if p.extension().and_then(|x| x.to_str()) == Some("json") {
                let Ok(mut sc) = crate::types::read_json::<SnapshotSidecar>(&p) else { continue };
                if sc.repo != rel {
                    sc.repo = rel.to_string();
                    crate::types::write_json(&p, &sc)?;
                }
            }
        }
        Ok(())
    }
    for sub in ["branch", "releases"] {
        walk(&repo_dir.join(sub), rel)?;
    }
    Ok(())
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

/// Parse a git tag as a release version. Tolerates a leading `v`/`V`, a
/// `release-`/`release/`/`release_` prefix, and a missing patch component
/// (`1.2` becomes `1.2.0`). Returns `None` for tags that are not versions.
pub fn release_version(tag: &str) -> Option<semver::Version> {
    let mut t = tag.trim();
    for prefix in ["release-", "release/", "release_"] {
        if let Some(rest) = t.strip_prefix(prefix) {
            t = rest;
            break;
        }
    }
    let t = t.trim_start_matches(['v', 'V']);
    if t.is_empty() {
        return None;
    }
    // normalize a missing patch so `1.2` parses; keep any pre-release/build
    // suffix untouched
    let core = t.split(['-', '+']).next().unwrap_or(t);
    let normalized = if core.matches('.').count() == 1 {
        match t.find(['-', '+']) {
            Some(i) => format!("{}.0{}", &t[..i], &t[i..]),
            None => format!("{t}.0"),
        }
    } else {
        t.to_string()
    };
    semver::Version::parse(&normalized).ok()
}

/// The version string to display and store for a release tag. Semver tags
/// become their canonical form; non-semver tags keep their raw name (minus a
/// leading `v`).
pub fn release_display_version(tag: &str) -> String {
    release_version(tag)
        .map(|v| v.to_string())
        .unwrap_or_else(|| tag.trim_start_matches(['v', 'V']).to_string())
}

/// Pick the highest semver-looking tag (`v` and `release-` prefixes tolerated).
pub fn latest_semver_tag(tags: &[String]) -> Option<String> {
    tags.iter()
        .filter_map(|t| release_version(t).map(|v| (v, t.clone())))
        .max_by(|a, b| a.0.cmp(&b.0))
        .map(|(_, t)| t)
}

/// The pacing host for a git remote, or `None` when there is nothing to pace:
/// a local path, `file://`, or an SSH remote, none of which are subject to
/// the forge's HTTP rate limits.
fn git_host(url: &str) -> Option<String> {
    let u = url.trim();
    (u.starts_with("http://") || u.starts_with("https://"))
        .then(|| crate::ratelimit::host_key(u))
}

pub struct Archiver {
    pub cfg: Config,
    /// Per-host request governor. CLI construction makes a private one; the
    /// server passes its process-wide governor to `with_governor_and_cache` so
    /// jobs share pacing state.
    pub governor: Arc<RemoteGovernor>,
    /// Conditional-request cache for forge API responses. Shared by the server
    /// across jobs, or loaded from the archive root by the CLI.
    pub http_cache: Arc<std::sync::Mutex<crate::httpcache::HttpCache>>,
}

impl Archiver {
    pub fn new(cfg: Config) -> Self {
        let cache = crate::httpcache::HttpCache::load(Path::new(&cfg.archive.root));
        Self {
            cfg,
            governor: Arc::new(RemoteGovernor::new()),
            http_cache: Arc::new(std::sync::Mutex::new(cache)),
        }
    }

    pub fn with_governor_and_cache(
        cfg: Config,
        governor: Arc<RemoteGovernor>,
        http_cache: Arc<std::sync::Mutex<crate::httpcache::HttpCache>>,
    ) -> Self {
        Self { cfg, governor, http_cache }
    }

    /// Add a repo at the archive root. Convenience wrapper around
    /// [`add_repo_in_folder`].
    #[tracing::instrument(skip(self, notes))]
    pub async fn add_repo(&self, url: &str, tags: &[String], notes: Option<String>) -> Result<PathBuf> {
        self.add_repo_in_folder(url, tags, notes, None).await
    }

    /// Add a repo, optionally under a category folder. The archive path
    /// (`folder/owner-repo`) is fixed before any snapshot is written, so the
    /// sidecars record the fully scoped location from the start instead of
    /// being moved after the fact.
    ///
    /// Fails if the repo is already archived (same URL or same owner-repo
    /// slug, wherever it lives). On any failure the partially-created repo
    /// directory is removed, so a retry starts clean instead of tripping over
    /// an orphaned `repo.json`.
    #[tracing::instrument(skip(self, notes))]
    pub async fn add_repo_in_folder(
        &self,
        url: &str,
        tags: &[String],
        notes: Option<String>,
        folder: Option<&str>,
    ) -> Result<PathBuf> {
        let info = forge::detect(url)?;
        let root = Path::new(&self.cfg.archive.root);
        // flat "owner-repo" leaf: fork collisions (same name, different
        // owners) live side by side instead of under owner folders
        let owner = sanitize(&info.owner);
        let name = sanitize(&info.name);
        let slug = format!("{owner}-{name}");
        let folder = folder
            .map(|f| f.trim().trim_matches('/'))
            .filter(|f| !f.is_empty());
        if let Some(f) = folder {
            if !f.split('/').all(crate::server::valid_folder_name) {
                bail!("invalid folder path: {f}");
            }
            // Materialize the folder chain so it stays visible after the repo
            // is later moved out or deleted.
            ensure_folder_manifests(root, f)?;
        }
        let rel = match folder {
            Some(f) => format!("{f}/{slug}"),
            None => slug.clone(),
        };
        let repo_dir = root.join(&rel);
        if repo_dir.join("repo.json").exists() {
            bail!("already archived: {} (use `refresh`)", repo_dir.display());
        }
        // The same owner-repo slug may already live under a category folder, or
        // have been added under a different URL spelling that resolves to the
        // same slug. Refuse duplicates with a cheap directory-name scan (no
        // sidecar parsing), so `add` stays cheap on a large archive.
        if let Some(existing) = crate::index::find_repo_by_slug(root, &slug) {
            let existing = existing.strip_prefix(root).unwrap_or(&existing).to_string_lossy();
            bail!("already archived as {existing} (use `refresh`)");
        }

        // Only auto-delete on failure if this add created the directory. If it
        // pre-existed (e.g. an unregistered orphan from `delete?files=false`),
        // leave the on-disk snapshots alone rather than destroying user data.
        let pre_existing = repo_dir.exists();
        match self
            .add_repo_inner(url, &info, &repo_dir, &rel, tags, notes)
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
        let (stars, suggested_tags, mut avatar_url) = match crate::forgeapi::github_repo_meta(&self.cfg, &self.governor, &self.http_cache, url).await {
            Some(meta) => {
                if description.is_none() {
                    description = meta.description;
                }
                (Some(meta.stars), meta.topics, meta.avatar_url)
            }
            None => (None, Vec::new(), None),
        };
        // GitLab and Forgejo icons come from their own API. GitHub already
        // resolved its avatar above, so this is a no-op there.
        if avatar_url.is_none() {
            avatar_url = crate::releaseapi::repo_avatar_url(&self.cfg, &self.governor, &self.http_cache, url).await;
        }

        // `[tags] folders`: the folder a repo lands in contributes its path
        // segments as tags (games/tools adds games and tools).
        let folder = rel.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let mut tags = tags.to_vec();
        crate::archiver::apply_folder_tags(&mut tags, "", folder, self.cfg.tags.folders);
        // "take on suggested tags": keep manual tags, then add the forge's
        // suggested topics (case-insensitively deduped) when enabled.
        let manifest_tags = if self.cfg.tags.take_suggested {
            crate::tagging::merge_suggested(&tags, &suggested_tags)
        } else {
            tags
        };

        let manifest = RepoManifest {
            origin: Some(url.to_string()),
            forge: info.kind.id().to_string(),
            name: info.name.clone(),
            added: now_rfc3339(),
            tags: manifest_tags,
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
            color: None,
            unidentified: false,
        };
        crate::types::write_json(&manifest_path, &manifest)?;

        // store the owner avatar locally so the repo icon survives the remote
        if self.cfg.fetch_icons() {
            if let Some(avatar) = avatar_url {
                if let Some(bytes) = crate::forgeapi::fetch_avatar(&self.cfg, &self.governor, &avatar).await {
                    let _ = fs::write(repo_dir.join("icon"), bytes);
                }
            }
        }

        if self.cfg.scheduler.run_on_add {
            self.snapshot_ref(repo_dir, git_dir, rel, &info.name, url, &branch, SnapshotKind::Branch, None, None, None)
                .await
                .context("failed to snapshot default branch")?;

            if let Some(tag) = self.latest_release_tag(url).await? {
                match self.fetch_tag(git_dir, url, &tag).await {
                    Ok(_) => {
                        let version = release_display_version(&tag);
                        let mut sc = self
                            .snapshot_ref(repo_dir, git_dir, rel, &info.name, url, &tag, SnapshotKind::Release, Some(&version), None, None)
                            .await
                            .context("failed to archive release")?;
                        if let Err(e) = self.sync_release_assets(repo_dir, &mut sc).await {
                            tracing::warn!(repo = %rel, error = %format!("{e:#}"), "release asset sync failed");
                        }
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

    /// Pace a network git operation through the shared governor. Local and
    /// `file://` remotes have no rate limit and are left alone. Returns an
    /// error when the host is paused longer than `[remote] max_wait_secs`, so
    /// the job fails and retries on the next scheduler pass instead of piling
    /// on more requests.
    async fn pace_remote(&self, url: &str) -> Result<()> {
        let Some(host) = git_host(url) else {
            return Ok(());
        };
        if self.governor.acquire(&host, &self.cfg.remote).await {
            return Ok(());
        }
        tracing::warn!(host = %host, "git remote is rate-limited; skipping this operation");
        bail!("host {host} is rate-limited; try again later")
    }

    async fn clone_shallow(&self, url: &str, dest: &Path) -> Result<()> {
        self.pace_remote(url).await?;
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
    async fn fetch_tag(&self, shallow: &Path, url: &str, tag: &str) -> Result<String> {
        self.pace_remote(url).await?;
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
    async fn ls_remote_latest_semver(&self, url: &str) -> Result<Option<String>> {
        self.pace_remote(url).await?;
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

    /// The tag of the release to archive. Semver git tags stay the primary
    /// source (offline, forge-agnostic). When a repo has no semver tag at all,
    /// fall back to the forge's own "latest release" so projects that tag with
    /// non-semver names (dates, codenames, `TDB...`) are archived too.
    async fn latest_release_tag(&self, url: &str) -> Result<Option<String>> {
        if let Some(tag) = self.ls_remote_latest_semver(url).await? {
            return Ok(Some(tag));
        }
        Ok(crate::releaseapi::latest_release_tag(
            &self.cfg,
            &self.governor,
            &self.http_cache,
            url,
        )
        .await)
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
        changelog_override: Option<&str>, // branch snapshots pass a compare readout
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

        // changelog: an explicit branch readout wins, else forge release notes
        // first, then CHANGELOG.md at the tag (releases only)
        let changelog = if let Some(c) = changelog_override {
            Some(c.chars().take(32 * 1024).collect::<String>())
        } else if matches!(kind, SnapshotKind::Release) {
            fetch_changelog(&self.cfg, &self.governor, &self.http_cache, origin, git_dir, label, version)
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
            assets: Vec::new(),
            assets_filters: Vec::new(),
            assets_max_mb: 0,
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
            // Link to a temp name, then swap it in. If the filesystem doesn't
            // support hard links (some volume/mount types), we keep the fresh
            // copy instead of failing the whole snapshot.
            let tmp = zip_path.with_extension("dedup.tmp");
            match fs::hard_link(&existing, &tmp) {
                Ok(()) => {
                    let _ = fs::rename(&tmp, &zip_path);
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    tracing::warn!(error = %e, "hard link dedup unsupported; keeping a second copy");
                }
            }
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

    /// Download release binaries matching the configured platforms and record
    /// them in the release sidecar. Existing files are kept when the remote
    /// size is unchanged, so refreshes don't re-download large files. Storage
    /// is `releases/<tag>/assets/`, next to the release snapshot.
    async fn sync_release_assets(
        &self,
        repo_dir: &Path,
        sidecar: &mut SnapshotSidecar,
    ) -> Result<usize> {
        if self.cfg.releases.platforms.is_empty() {
            return Ok(0);
        }
        if sidecar.origin.is_empty() || sidecar.r#ref.is_empty() {
            return Ok(0);
        }
        // Canonical, order-preserving filter set. Recorded in the sidecar so a
        // no-op refresh does not hit the release API again (GitHub allows only
        // 60 unauthenticated requests per hour).
        let mut filters: Vec<String> = Vec::new();
        for p in &self.cfg.releases.platforms {
            if let Some(c) = crate::platform::canonical(p) {
                if !filters.contains(&c) {
                    filters.push(c);
                }
            }
        }
        if sidecar.assets_filters == filters && sidecar.assets_max_mb == self.cfg.releases.max_asset_mb {
            return Ok(0);
        }
        let Some(release) =
            crate::releaseapi::fetch_release(&self.cfg, &self.governor, &self.http_cache, &sidecar.origin, &sidecar.r#ref).await
        else {
            // transient (rate limit, network): leave assets_filters untouched so
            // the next poll retries instead of caching the miss
            return Ok(0);
        };

        let release_dir = repo_dir.join("releases").join(sanitize(&sidecar.r#ref));
        let assets_dir = release_dir.join("assets");
        let max_bytes = self.cfg.releases.max_asset_mb.saturating_mul(1024 * 1024);

        // remove half-written downloads left by a killed process; the next
        // attempt recreates them from scratch
        if let Ok(entries) = fs::read_dir(&assets_dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("part") {
                    let _ = fs::remove_file(&p);
                }
            }
        }

        let mut stored = 0usize;
        for asset in &release.assets {
            let remote_name = asset.effective_name();
            let Some(platform) = crate::platform::classify(&remote_name) else {
                continue;
            };
            if !crate::platform::matches_any(&self.cfg.releases.platforms, platform) {
                continue;
            }
            let file_name = sanitize(&remote_name);
            if file_name.is_empty() || file_name == "." || file_name == ".." {
                continue;
            }
            let dest = assets_dir.join(&file_name);

            // keep an existing download when the remote size is unchanged
            if let Some(existing) = sidecar.assets.iter().find(|a| a.name == file_name) {
                if dest.is_file() && asset.size.is_none_or(|sz| sz == existing.bytes) {
                    stored += 1;
                    continue;
                }
            }
            if let Some(sz) = asset.size {
                if max_bytes > 0 && sz > max_bytes {
                    tracing::debug!(asset = %remote_name, bytes = sz, "skipping oversized release asset");
                    continue;
                }
            }

            match crate::releaseapi::download_asset(
                &self.cfg,
                &self.governor,
                &sidecar.origin,
                asset,
                &dest,
                max_bytes,
            )
            .await
            {
                Ok((bytes, sha256)) => {
                    sidecar.assets.retain(|a| a.name != file_name);
                    sidecar.assets.push(crate::types::StoredAsset {
                        name: file_name.clone(),
                        platform: platform.slug(),
                        url: asset.url.clone(),
                        bytes,
                        sha256,
                        downloaded_at: now_rfc3339(),
                    });
                    stored += 1;
                    tracing::info!(asset = %remote_name, platform = %platform.slug(), bytes, "stored release asset");
                }
                Err(e) => {
                    tracing::warn!(asset = %remote_name, error = %format!("{e:#}"), "release asset download failed");
                }
            }
        }

        // drop metadata for files that are no longer on disk
        sidecar.assets.retain(|a| assets_dir.join(&a.name).is_file());

        // Always persist the filter set, so a release with no matching assets
        // is not re-queried on every poll.
        sidecar.assets_filters = filters;
        sidecar.assets_max_mb = self.cfg.releases.max_asset_mb;
        let json_path = release_dir.join(&sidecar.zip.file).with_extension("json");
        crate::types::write_json(&json_path, sidecar)?;
        Ok(stored)
    }

    /// Refresh one repo against its remote: re-snapshot the default branch if
    /// it moved, archive a newer semver release if one exists, prune per
    /// retention, and record the check time
    #[tracing::instrument(skip(self))]
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

        // backfill the owner avatar for archives created before icon support.
        // GitHub resolves its avatar from the repo metadata call; GitLab and
        // Forgejo use their own project API. A zero-byte `icon` marks
        // "checked, no avatar" so the API is not retried.
        if self.cfg.fetch_icons() && !repo_dir.join("icon").exists() {
            let info = crate::forge::detect(&origin).ok();
            match info.as_ref().map(|i| i.kind) {
                Some(crate::forge::ForgeKind::GitHub) => {
                    if let Some(meta) = crate::forgeapi::github_repo_meta(&self.cfg, &self.governor, &self.http_cache, &origin).await {
                        match meta.avatar_url {
                            Some(url) => {
                                if let Some(bytes) = crate::forgeapi::fetch_avatar(&self.cfg, &self.governor, &url).await {
                                    let _ = fs::write(repo_dir.join("icon"), bytes);
                                }
                            }
                            None => {
                                let _ = fs::write(repo_dir.join("icon"), b"");
                            }
                        }
                    }
                }
                Some(crate::forge::ForgeKind::GitLab | crate::forge::ForgeKind::Forgejo) => {
                    if let Some(url) = crate::releaseapi::repo_avatar_url(&self.cfg, &self.governor, &self.http_cache, &origin).await {
                        if let Some(bytes) = crate::forgeapi::fetch_avatar(&self.cfg, &self.governor, &url).await {
                            let _ = fs::write(repo_dir.join("icon"), bytes);
                        }
                    }
                }
                // generic remotes and unidentified imports have no avatar API
                _ => {}
            }
        }

        // Probe the remote first (cheap). If it's gone entirely, record that
        // state on the manifest and keep the local archive untouched.
        self.pace_remote(&origin).await?;
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

        self.pace_remote(&origin).await?;
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
            let archived = archived_release_tags(repo_dir);
            if archived.iter().any(|t| t == tag) {
                false
            } else {
                match (release_version(tag), archived_latest_release(repo_dir)) {
                    (Some(n), Some(o)) => n > o,
                    (Some(_), None) => true,
                    // a non-semver tag: the forge's latest release is new
                    (None, _) => true,
                }
            }
        } else {
            false
        };

        // Keep release binaries in sync even when nothing moved, which also
        // backfills archives created before platform filters were set.
        if !release_needed && !self.cfg.releases.platforms.is_empty() {
            if let Some(mut sc) = latest_release_sidecar(repo_dir) {
                if let Err(e) = self.sync_release_assets(repo_dir, &mut sc).await {
                    tracing::warn!(repo = %rel, error = %format!("{e:#}"), "release asset sync failed");
                }
            }
        }

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

        // branch snapshot, with a commit-level changelog when the forge can
        // compare the previous and new commits (GitHub only for now)
        if branch_changed {
            let changelog = if !local_sha.is_empty() && !remote_branch_sha.is_empty() {
                match crate::forgeapi::github_compare(&self.cfg, &self.governor, &self.http_cache, &origin, &local_sha, &remote_branch_sha).await {
                    Some(info) => {
                        summary.branch_commits = Some(info.total_commits);
                        Some(render_branch_changelog(&info))
                    }
                    None => None,
                }
            } else {
                None
            };
            self.snapshot_ref(repo_dir, git_dir, &rel, &name, &origin, &branch, SnapshotKind::Branch, None, None, changelog.as_deref())
                .await?;
            summary.branch_changelog = changelog;
            summary.new_branch_snapshot = true;
        }

        // release snapshot
        if release_needed {
            if let Some(tag) = new_release_tag {
                if self.fetch_tag(git_dir, &origin, &tag).await.is_ok() {
                    let version = release_display_version(&tag);
                    let mut sc = self
                        .snapshot_ref(repo_dir, git_dir, &rel, &name, &origin, &tag, SnapshotKind::Release, Some(&version), None, None)
                        .await?;
                    if let Err(e) = self.sync_release_assets(repo_dir, &mut sc).await {
                        tracing::warn!(repo = %rel, error = %format!("{e:#}"), "release asset sync failed");
                    }
                    summary.new_release = Some(version);
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
        self.pace_remote(origin).await?;
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
                .and_then(release_version)
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

/// The most recently archived release sidecar (its assets, changelog, tag).
fn latest_release_sidecar(repo_dir: &Path) -> Option<SnapshotSidecar> {
    let entries = fs::read_dir(repo_dir.join("releases")).ok()?;
    let mut best: Option<SnapshotSidecar> = None;
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        let Some(sc) = find_sidecar(&p) else { continue };
        if best.as_ref().is_none_or(|b| sc.archived_at > b.archived_at) {
            best = Some(sc);
        }
    }
    best
}

/// Tags of every release already archived (the sidecar `ref`), used to tell
/// whether the remote's latest release is new even when its tag is not semver.
fn archived_release_tags(repo_dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(repo_dir.join("releases")) else { return out };
    for e in entries.flatten() {
        let p = e.path();
        if !p.is_dir() {
            continue;
        }
        if let Some(sc) = find_sidecar(&p) {
            out.push(sc.r#ref);
        }
    }
    out
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
            if let Some(v) = sc.version.as_deref().and_then(release_version) {
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
    fn git_host_only_paces_http_remotes() {
        assert_eq!(git_host("https://github.com/o/r").as_deref(), Some("github.com"));
        assert_eq!(git_host("http://git.internal:8080/o/r").as_deref(), Some("git.internal:8080"));
        assert!(git_host("file:///tmp/remote").is_none());
        assert!(git_host("/tmp/local/repo").is_none());
        assert!(git_host("git@github.com:o/r.git").is_none());
    }

    #[tokio::test]
    async fn git_pacing_skips_local_and_spaces_network() {
        let mut cfg = Config::default();
        cfg.remote.min_interval_ms = 100_000;
        cfg.remote.jitter_ms = 0;
        cfg.remote.max_wait_secs = 1;
        let archiver = Archiver::new(cfg);
        // Local and file remotes are never paced.
        assert!(archiver.pace_remote("file:///tmp/remote").await.is_ok());
        assert!(archiver.pace_remote("/tmp/local").await.is_ok());
        // A network origin is paced: the first call is fine, an immediate
        // second one is skipped rather than waiting out the interval.
        assert!(archiver.pace_remote("https://github.com/o/r").await.is_ok());
        assert!(archiver.pace_remote("https://github.com/o/r").await.is_err());
    }

    #[tokio::test]
    async fn git_pacing_disabled_is_a_noop() {
        let mut cfg = Config::default();
        cfg.remote.enabled = false;
        cfg.remote.min_interval_ms = 100_000;
        let archiver = Archiver::new(cfg);
        assert!(archiver.pace_remote("https://github.com/o/r").await.is_ok());
        assert!(archiver.pace_remote("https://github.com/o/r").await.is_ok());
    }

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
    fn release_version_tolerates_common_tag_schemes() {
        let parse = |t: &str| release_version(t).map(|v| v.to_string());
        assert_eq!(parse("v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse("1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse("release-1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse("release/v1.2.3").as_deref(), Some("1.2.3"));
        assert_eq!(parse("v1.2").as_deref(), Some("1.2.0"), "a missing patch is padded");
        assert_eq!(parse("1.2").as_deref(), Some("1.2.0"));
        assert_eq!(parse("2.0.0-rc.1").as_deref(), Some("2.0.0-rc.1"));
        assert_eq!(parse("1.2-beta").as_deref(), Some("1.2.0-beta"));
        assert!(parse("TDB335.24041").is_none());
        assert!(parse("nightly").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn latest_semver_accepts_release_prefix_and_two_part_versions() {
        let tags = vec![
            "release-1.2.3".into(),
            "v1.10".into(),
            "nightly".into(),
        ];
        assert_eq!(latest_semver_tag(&tags).as_deref(), Some("v1.10"));
    }

    #[test]
    fn release_display_version_keeps_non_semver_names() {
        assert_eq!(release_display_version("v1.2.3"), "1.2.3");
        assert_eq!(release_display_version("release-1.2.3"), "1.2.3");
        assert_eq!(release_display_version("TDB335.24041"), "TDB335.24041");
        assert_eq!(release_display_version("vTDB"), "TDB");
    }

    #[test]
    fn rewrite_sidecar_repo_updates_every_sidecar_and_skips_assets() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("old-leaf");
        let branch = repo.join("branch").join("master");
        let release = repo.join("releases").join("v1.0.0");
        std::fs::create_dir_all(release.join("assets")).unwrap();
        std::fs::create_dir_all(&branch).unwrap();
        let sidecar = |repo_field: &str, kind: &str| {
            serde_json::json!({
                "kind": kind,
                "repo": repo_field,
                "origin": "https://github.com/o/r",
                "ref": "v1.0.0",
                "commit": "abc",
                "archived_at": "2025-01-01T00:00:00Z",
                "archiver_version": "t",
                "zip": { "file": "r-v1.0.0.zip", "bytes": 1, "sha256": "x" }
            })
            .to_string()
        };
        std::fs::write(branch.join("b.json"), sidecar("old-leaf", "branch-snapshot")).unwrap();
        std::fs::write(release.join("r.json"), sidecar("old-leaf", "release")).unwrap();
        // a decoy under assets/ must not be rewritten
        std::fs::write(release.join("assets").join("decoy.json"), sidecar("old-leaf", "release")).unwrap();

        rewrite_sidecar_repo(&repo, "tools/new-leaf").unwrap();

        let b = crate::types::read_json::<SnapshotSidecar>(&branch.join("b.json")).unwrap();
        let r = crate::types::read_json::<SnapshotSidecar>(&release.join("r.json")).unwrap();
        assert_eq!(b.repo, "tools/new-leaf");
        assert_eq!(r.repo, "tools/new-leaf");
        let decoy: serde_json::Value =
            crate::types::read_json(&release.join("assets").join("decoy.json")).unwrap();
        assert_eq!(decoy["repo"], "old-leaf", "asset decoys are untouched");
    }

    #[test]
    fn sanitize_keeps_safe_chars() {
        assert_eq!(sanitize("feature/abc-def"), "feature_abc-def");
        assert_eq!(sanitize("ripgrep"), "ripgrep");
    }

    #[test]
    fn folder_tags_splits_a_path_into_segments() {
        assert_eq!(folder_tags("games/tools"), vec!["games", "tools"]);
        assert_eq!(folder_tags("/games//tools/"), vec!["games", "tools"]);
        assert!(folder_tags("").is_empty());
    }

    #[test]
    fn apply_folder_tags_adds_new_folder_and_drops_the_old_one() {
        // entering a folder adds its segments
        let mut tags = vec!["manual".to_string()];
        apply_folder_tags(&mut tags, "", "games/tools", true);
        assert_eq!(tags, vec!["games", "manual", "tools"]);

        // moving between folders swaps the location-derived tags, keeping manual ones
        let mut tags = vec!["games".to_string(), "manual".to_string(), "tools".to_string()];
        apply_folder_tags(&mut tags, "games/tools", "apps/tools", true);
        assert_eq!(tags, vec!["apps", "manual", "tools"], "games dropped, tools kept, apps added");

        // disabled: nothing changes
        let mut tags = vec!["manual".to_string()];
        apply_folder_tags(&mut tags, "", "games/tools", false);
        assert_eq!(tags, vec!["manual"]);
    }

    #[test]
    fn ensure_folder_manifests_creates_chain_and_keeps_existing_icons() {
        let tmp = tempfile::tempdir().unwrap();
        ensure_folder_manifests(tmp.path(), "a/b/c").unwrap();
        for p in ["a", "a/b", "a/b/c"] {
            assert!(tmp.path().join(p).join("folder.json").exists(), "{p} missing manifest");
        }
        // an existing manifest is not overwritten
        let custom = crate::types::FolderManifest { icon: Some("🎮".into()) };
        crate::types::write_json(&tmp.path().join("a/b/folder.json"), &custom).unwrap();
        ensure_folder_manifests(tmp.path(), "a/b/c").unwrap();
        let back: crate::types::FolderManifest =
            crate::types::read_json(&tmp.path().join("a/b/folder.json")).unwrap();
        assert_eq!(back.icon.as_deref(), Some("🎮"));
    }

    #[test]
    fn sweep_empty_folders_removes_implicit_but_keeps_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("implicit/deep")).unwrap();
        std::fs::create_dir_all(root.join("explicit/deep")).unwrap();
        crate::types::write_json(
            &root.join("explicit/folder.json"),
            &crate::types::FolderManifest::default(),
        )
        .unwrap();

        sweep_empty_folders(root, &root.join("implicit/deep"));
        assert!(!root.join("implicit").exists(), "empty implicit folders are swept");

        // an implicit child of an explicit folder is swept, but the explicit
        // folder itself survives, and sweeping it directly is a no-op
        sweep_empty_folders(root, &root.join("explicit/deep"));
        assert!(!root.join("explicit/deep").exists());
        sweep_empty_folders(root, &root.join("explicit"));
        assert!(root.join("explicit").exists(), "explicit empty folder is kept");
    }

    #[test]
    fn sanitize_replaces_unsafe_chars_and_keeps_safe_ones() {
        assert_eq!(sanitize(""), "");
        assert_eq!(sanitize("a b/c:d"), "a_b_c_d");
        assert_eq!(sanitize("caf\u{e9}"), "caf_", "non-ascii becomes an underscore");
        assert_eq!(sanitize(".._etc"), ".._etc", "dots are preserved");
    }

    #[test]
    fn branch_changelog_lists_commits_and_summary() {
        let info = crate::forgeapi::CompareInfo {
            total_commits: 2,
            files_changed: 5,
            html_url: "https://github.com/o/r/compare/a...b".into(),
            commits: vec![
                crate::forgeapi::CompareCommit {
                    sha: "abc1234".into(),
                    message: "fix the thing".into(),
                    author: "Alice".into(),
                    date: "2026-10-01".into(),
                },
                crate::forgeapi::CompareCommit {
                    sha: "def5678".into(),
                    message: "add feature".into(),
                    author: "Bob".into(),
                    date: "2026-10-02".into(),
                },
            ],
        };
        let md = render_branch_changelog(&info);
        assert!(md.contains("**2 commit(s)**"), "{md}");
        assert!(md.contains("**5 file(s) changed**"), "{md}");
        assert!(md.contains("abc1234"), "{md}");
        assert!(md.contains("add feature"), "{md}");
    }

    /// With no configured platforms, asset syncing must not touch the network
    /// or the sidecar at all.
    #[tokio::test]
    async fn asset_sync_is_a_noop_without_platforms() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        let archiver = Archiver::new(cfg);
        let mut sc: SnapshotSidecar = serde_json::from_value(serde_json::json!({
            "kind": "release",
            "repo": "o-r",
            "origin": "https://github.com/o/r",
            "ref": "v1.0.0",
            "commit": "abc",
            "archived_at": "2025-01-01T00:00:00Z",
            "archiver_version": "test",
            "zip": { "file": "r-v1.0.0.zip", "bytes": 1, "sha256": "x" }
        }))
        .unwrap();
        assert_eq!(archiver.sync_release_assets(tmp.path(), &mut sc).await.unwrap(), 0);
        assert!(sc.assets.is_empty());
    }

    /// A configured filter plus an unsupported origin is still a clean no-op
    /// (no error, no downloads) rather than a network call.
    #[tokio::test]
    async fn asset_sync_skips_unsupported_forges() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        cfg.releases.platforms = vec!["linux-x64".into()];
        let archiver = Archiver::new(cfg);
        let mut sc: SnapshotSidecar = serde_json::from_value(serde_json::json!({
            "kind": "release",
            "repo": "o-r",
            "origin": "file:///tmp/fixture/remote",
            "ref": "v1.0.0",
            "commit": "abc",
            "archived_at": "2025-01-01T00:00:00Z",
            "archiver_version": "test",
            "zip": { "file": "r-v1.0.0.zip", "bytes": 1, "sha256": "x" }
        }))
        .unwrap();
        assert_eq!(archiver.sync_release_assets(tmp.path(), &mut sc).await.unwrap(), 0);
    }

    /// Once a release has been synced for the current filters, a refresh must
    /// not call the release API again. A real-looking GitHub origin proves the
    /// short-circuit happens before any network access.
    #[tokio::test]
    async fn asset_sync_short_circuits_when_filters_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        cfg.releases.platforms = vec!["linux-x64".into()];
        let archiver = Archiver::new(cfg);
        let mut sc: SnapshotSidecar = serde_json::from_value(serde_json::json!({
            "kind": "release",
            "repo": "o-r",
            "origin": "https://github.com/o/r",
            "ref": "v1.0.0",
            "commit": "abc",
            "archived_at": "2025-01-01T00:00:00Z",
            "archiver_version": "test",
            "assets_filters": ["linux-x64"],
            "zip": { "file": "r-v1.0.0.zip", "bytes": 1, "sha256": "x" }
        }))
        .unwrap();
        assert_eq!(archiver.sync_release_assets(tmp.path(), &mut sc).await.unwrap(), 0);
    }
}