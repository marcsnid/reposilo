//! HTTP API + background scheduler
//!
//! The API is a thin layer over the archiver and the disk-rebuildable index.
//! Long-running work (add/refresh) runs as in-memory jobs so requests return
//! 202 immediately. Notifications ("new version archived") are in-memory for
//! now; everything durable already lives on disk in sidecars/manifests.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::{Mutex, OwnedSemaphorePermit, RwLock, Semaphore};

use crate::archiver::{now_rfc3339, Archiver};
use crate::importer;
use crate::config::Config;
use crate::index::{Index, RepoEntry, SnapshotEntry};
use crate::types::write_json;

/// Cap on retained job / job-note entries (long-running memory bound).
const MAX_TRACKED_JOBS: usize = 500;
/// How long a parsed archive listing stays cached (a browsing session).
const LISTING_TTL: std::time::Duration = std::time::Duration::from_secs(300);
/// Hard cap on cached listings, so memory can't grow unboundedly.
const LISTING_CACHE_MAX: usize = 32;

// ---------- state ----------

#[derive(Debug, Clone, Serialize)]
pub struct Job {
    pub id: u64,
    pub kind: String, // "add" | "manual-refresh" | "scheduled-refresh"
    pub repo: Option<String>,
    pub status: String, // "running" | "done" | "failed"
    pub error: Option<String>,
    pub created: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: u64,
    pub kind: String, // new_release | new_snapshot | remote_gone | repo_added | import | autotag
    pub repo: String,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>, // markdown (changelogs for new_release)
    pub at: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct NotificationsFile {
    next_id: u64,
    items: Vec<Notification>,
}

pub struct AppState {
    /// Live config: settings edits apply immediately (and are persisted).
    pub cfg: RwLock<Config>,
    /// Where the config file lives (for the settings UI to save back to).
    pub config_path: Option<PathBuf>,
    pub index: RwLock<Index>,
    pub jobs: Mutex<HashMap<u64, Job>>,
    /// Repo keys (folder basenames) currently being worked on. A plain std
    /// mutex so a `RepoLockGuard` can release it from `Drop` (no await).
    pub running: std::sync::Mutex<HashSet<String>>,
    pub notifications: Mutex<Vec<Notification>>,
    /// Stored import scans for the review UI (keyed by scan id).
    pub scans: Mutex<HashMap<u64, importer::ImportScan>>,
    /// User accounts (users.json in the archive root, reloaded on mtime).
    pub users: std::sync::Mutex<crate::auth::Users>,
    /// Cookie sessions (in-memory; restart requires re-login).
    pub sessions: crate::auth::SessionStore,
    /// Optional summary strings shown by the job-status endpoint.
    pub job_notes: Mutex<HashMap<u64, String>>,
    /// Short-lived parsed-archive-listing cache. Building a listing is free for
    /// zip (central directory) but a full decompress for tar.zst, so the UI
    /// loads it once per repo page and reuses it; entries expire on their own.
    pub listing_cache: Mutex<HashMap<String, (std::time::Instant, Arc<crate::files::ArchiveIndex>)>>,
    /// Runtime + per-day stats for the built-in stats page and OTLP.
    pub metrics: Mutex<crate::metrics::Metrics>,
    next_job_id: AtomicU64,
    next_scan_id: AtomicU64,
}

impl AppState {
    pub async fn new(mut cfg: Config, config_path: Option<PathBuf>) -> Result<Self> {
        cfg.with_absolute_root();
        let root = PathBuf::from(&cfg.archive.root);
        tokio::fs::create_dir_all(&root)
            .await
            .with_context(|| format!("cannot create archive root {}", root.display()))?;
        if let Ok(c) = root.canonicalize() {
            cfg.archive.root = c.to_string_lossy().into_owned();
        }
        let index = Index::load(&root)?;
        let notifications: Vec<Notification> = crate::types::read_json::<NotificationsFile>(&root.join("notifications.json"))
            .map(|f| f.items)
            .unwrap_or_default();
        Ok(Self {
            cfg: RwLock::new(cfg),
            config_path,
            index: RwLock::new(index),
            jobs: Mutex::new(HashMap::new()),
            running: std::sync::Mutex::new(HashSet::new()),
            notifications: Mutex::new(notifications),
            scans: Mutex::new(HashMap::new()),
            job_notes: Mutex::new(HashMap::new()),
            listing_cache: Mutex::new(HashMap::new()),
            metrics: Mutex::new(crate::metrics::Metrics::load(&root)),
            users: std::sync::Mutex::new(crate::auth::Users::load(&root)),
            sessions: crate::auth::SessionStore::new(),
            next_job_id: AtomicU64::new(1),
            next_scan_id: AtomicU64::new(1),
        })
    }

    /// Clone of the current (live) config.
    pub async fn cfg(&self) -> Config {
        self.cfg.read().await.clone()
    }

    /// Store a scan, keeping only the most recent 8.
    pub async fn store_scan(&self, scan: importer::ImportScan) -> u64 {
        let id = self.next_scan_id.fetch_add(1, Ordering::Relaxed);
        let mut scans = self.scans.lock().await;
        if scans.len() >= 8 {
            let oldest = scans.keys().min().copied();
            if let Some(k) = oldest {
                scans.remove(&k);
            }
        }
        scans.insert(id, scan);
        id
    }

    pub async fn root(&self) -> PathBuf {
        PathBuf::from(&self.cfg().await.archive.root)
    }

    /// Rebuild the index from disk and swap it in. The disk walk + JSON parse
    /// can take tens of ms on a large archive, so it runs on a blocking thread
    /// to avoid stalling the async executor (this runs after every job/edit).
    pub async fn reindex(&self) -> Result<()> {
        let root = self.root().await;
        let fresh = tokio::task::spawn_blocking(move || Index::load(&root))
            .await
            .map_err(|e| anyhow::anyhow!("reindex task failed: {e}"))??;
        *self.index.write().await = fresh;
        Ok(())
    }

    pub(crate) async fn create_job(&self, kind: &str, repo: Option<String>) -> u64 {
        let id = self.next_job_id.fetch_add(1, Ordering::Relaxed);
        let job = Job {
            id,
            kind: kind.into(),
            repo,
            status: "running".into(),
            error: None,
            created: now_rfc3339(),
        };
        let mut jobs = self.jobs.lock().await;
        jobs.insert(id, job);
        // Bound memory for a process that runs for months: keep only the newest
        // MAX_TRACKED_JOBS entries, evicting the oldest ids first.
        if jobs.len() > MAX_TRACKED_JOBS {
            let mut ids: Vec<u64> = jobs.keys().copied().collect();
            ids.sort_unstable();
            let excess = jobs.len() - MAX_TRACKED_JOBS;
            for old in ids.into_iter().take(excess) {
                jobs.remove(&old);
            }
        }
        id
    }

    /// Record a human summary for a job, keeping `job_notes` bounded too.
    pub async fn set_job_note(&self, id: u64, note: String) {
        let mut notes = self.job_notes.lock().await;
        notes.insert(id, note);
        if notes.len() > MAX_TRACKED_JOBS {
            let mut ids: Vec<u64> = notes.keys().copied().collect();
            ids.sort_unstable();
            let excess = notes.len() - MAX_TRACKED_JOBS;
            for old in ids.into_iter().take(excess) {
                notes.remove(&old);
            }
        }
    }

    pub(crate) async fn finish_job(&self, id: u64, error: Option<String>) {
        let mut jobs = self.jobs.lock().await;
        if let Some(job) = jobs.get_mut(&id) {
            job.status = if error.is_some() { "failed".into() } else { "done".into() };
            job.error = error;
        }
    }

    /// Locks are keyed by the repo's unique folder basename (the owner-repo
    /// slug). Refresh/delete/patch pass the full relative path, add passes the
    /// bare slug; both must resolve to the same key so they can never race.
    fn lock_key(rel: &str) -> &str {
        rel.rsplit('/').next().unwrap_or(rel)
    }

    /// Mark a repo as being worked on; false means something is already running.
    pub async fn try_lock_repo(&self, rel: &str) -> bool {
        self.running.lock().unwrap().insert(Self::lock_key(rel).to_string())
    }

    pub async fn job(&self, id: u64) -> Option<Job> {
        self.jobs.lock().await.get(&id).cloned()
    }

    /// Recorded summary for a finished job, if any (used by job-status UI).
    pub async fn job_note(&self, id: u64) -> Option<String> {
        self.job_notes.lock().await.get(&id).cloned()
    }

    pub async fn unlock_repo(&self, rel: &str) {
        self.running.lock().unwrap().remove(Self::lock_key(rel));
    }

    pub async fn push_notification(&self, kind: &str, repo: &str, title: String, body: Option<String>) {
        let n = Notification {
            id: 0, // assigned below
            kind: kind.into(),
            repo: repo.into(),
            title,
            body,
            at: now_rfc3339(),
        };
        // Resolve the root before taking the notifications lock: `root()` awaits
        // the config lock, and we don't want to hold one lock across another.
        let notif_path = self.root().await.join("notifications.json");
        let mut items = self.notifications.lock().await;
        let id = items.first().map(|i| i.id + 1).unwrap_or(1);
        let n = Notification { id, ..n };
        items.insert(0, n.clone());
        if items.len() > 500 {
            items.truncate(500);
        }
        let file = NotificationsFile {
            next_id: id + 1,
            items: items.clone(),
        };
        let _ = write_json(&notif_path, &file);
        drop(items);
        self.fire_webhook(&n).await;
    }

    async fn fire_webhook(&self, n: &Notification) {
        let url = self.cfg().await.notifications.webhook_url.clone();
        let Some(url) = url else { return };
        if !matches!(n.kind.as_str(), "new_release" | "remote_gone") {
            return; // webhook only fires on releases + dead links
        }
        let payload = serde_json::json!({
            "event": n.kind, "repo": n.repo, "title": n.title,
            "body": n.body, "at": n.at,
        });
        let st2 = self;
        let _n = n.clone();
        tokio::spawn(async move {
            let _ = st2; // keep Arc alive
            if let Err(e) = reqwest::Client::new()
                .post(&url)
                .json(&payload)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                tracing::warn!(url = %url, error = %e, "webhook delivery failed");
            }
        });
    }

    pub async fn notification_count(&self, since: Option<&str>) -> usize {
        let items = self.notifications.lock().await;
        match since {
            Some(s) if !s.is_empty() => items
                .iter()
                .filter(|n| n.at.as_str() > s)
                .count(),
            _ => items.len(),
        }
    }

    /// Get (building + caching if needed) the parsed listing for an archive.
    /// The cache is time-bounded and size-bounded; it never grows unbounded and
    /// never leaves files behind.
    pub async fn archive_index(
        &self,
        path: &std::path::Path,
    ) -> Option<Arc<crate::files::ArchiveIndex>> {
        let key = path.to_string_lossy().into_owned();
        let now = std::time::Instant::now();
        {
            let mut cache = self.listing_cache.lock().await;
            cache.retain(|_, (t, _)| now.duration_since(*t) < LISTING_TTL);
            if let Some((t, idx)) = cache.get_mut(&key) {
                *t = now;
                return Some(idx.clone());
            }
        }
        let p = path.to_path_buf();
        let idx = tokio::task::spawn_blocking(move || crate::files::ArchiveIndex::build(&p))
            .await
            .ok()??;
        let idx = Arc::new(idx);
        let mut cache = self.listing_cache.lock().await;
        if cache.len() >= LISTING_CACHE_MAX {
            if let Some(oldest) = cache.iter().min_by_key(|(_, (t, _))| *t).map(|(k, _)| k.clone()) {
                cache.remove(&oldest);
            }
        }
        cache.insert(key, (now, idx.clone()));
        Some(idx)
    }

    /// Find a repo by rel path; returns a clone so no lock is held.
    pub async fn find_repo(&self, rel: &str) -> Option<RepoEntry> {
        self.index.read().await.find(rel).cloned()
    }

    /// Bump one or more metric counters and persist (small JSON, cheap).
    pub async fn record(&self, fields: &[&str]) {
        if fields.is_empty() {
            return;
        }
        let root = self.root().await;
        let mut m = self.metrics.lock().await;
        for f in fields {
            m.bump(f);
        }
        m.prune();
        m.save(&root);
    }

    /// Current index-derived gauges (repos, snapshots, dead/unavailable...).
    pub async fn totals(&self) -> crate::metrics::Totals {
        let index = self.index.read().await;
        let mut t = crate::metrics::Totals::default();
        for r in &index.repos {
            t.repos += 1;
            t.snapshots += r.branch_snapshots.len() as u64;
            t.releases += r.releases.len() as u64;
            match r.manifest.remote_state.as_deref() {
                Some("dead") => t.dead += 1,
                Some("unavailable") => t.unavailable += 1,
                _ => {}
            }
            if r.manifest.tags.is_empty() {
                t.untagged += 1;
            }
        }
        t
    }

    /// A consistent (metrics, totals) snapshot for the stats page / OTLP.
    pub async fn stats_snapshot(&self) -> (crate::metrics::Metrics, crate::metrics::Totals) {
        let metrics = self.metrics.lock().await.clone();
        let totals = self.totals().await;
        (metrics, totals)
    }
}

// ---------- errors ----------

/// Releases a repo lock on drop. Owned by a spawned job so a panic or an early
/// exit can never leave the repo permanently locked.
pub struct RepoLockGuard {
    st: Arc<AppState>,
    key: String,
}

impl RepoLockGuard {
    pub fn new(st: Arc<AppState>, rel: &str) -> Self {
        Self { st, key: AppState::lock_key(rel).to_string() }
    }
}

impl Drop for RepoLockGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.st.running.lock() {
            set.remove(&self.key);
        }
    }
}

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    fn internal(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
    fn not_found(msg: impl Into<String>) -> Self {
        Self(StatusCode::NOT_FOUND, msg.into())
    }
    fn bad_request(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn conflict(msg: impl Into<String>) -> Self {
        Self(StatusCode::CONFLICT, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn ok_json(v: Value) -> Response {
    Json(v).into_response()
}

// ---------- JSON rendering ----------

fn snapshot_rel_path(repo: &RepoEntry, e: &SnapshotEntry) -> String {
    let subdir = e
        .dir
        .strip_prefix(&repo.dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{subdir}/{}", e.sidecar.zip.file)
}

fn snapshot_json(repo: &RepoEntry, e: &SnapshotEntry) -> Value {
    let subdir = e
        .dir
        .strip_prefix(&repo.dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let assets: Vec<Value> = e
        .sidecar
        .assets
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "platform": a.platform,
                "platform_label": crate::platform::describe(&a.platform),
                "bytes": a.bytes,
                "sha256": a.sha256,
                "download": format!("/api/repos/{}/asset/{subdir}/assets/{}", repo.rel, a.name),
            })
        })
        .collect();
    json!({
        "kind": e.sidecar.kind,
        "ref": e.sidecar.r#ref,
        "version": e.sidecar.version,
        "commit": e.sidecar.commit,
        "archived_at": e.sidecar.archived_at,
        "committed_at": e.sidecar.committed_at,
        "bytes": e.sidecar.zip.bytes,
        "file": e.sidecar.zip.file,
        "download": format!("/api/repos/{}/archive/{}", repo.rel, snapshot_rel_path(repo, e)),
        "assets": assets,
    })
}

fn repo_json(repo: &RepoEntry, detail: bool) -> Value {
    let m = &repo.manifest;
    let base = json!({
        "path": repo.rel,
        "name": m.name,
        "origin": m.origin,
        "forge": m.forge,
        "tags": m.tags,
        "language": m.language,
        "description": m.description,
        "notes": m.notes,
        "default_branch": m.default_branch,
        "remote_state": m.remote_state,
        "last_checked": m.last_checked,
        "added": m.added,
        "schedule": { "interval_days": m.schedule.interval_days },
        "retention": {
            "keep_branch_snapshots": m.retention.keep_branch_snapshots,
            "keep_releases": m.retention.keep_releases,
        },
        "branch_snapshot_count": repo.branch_snapshots.len(),
        "release_count": repo.releases.len(),
        "url": format!("/api/repos/{}", repo.rel),
    });
    if !detail {
        return base;
    }
    let mut v = base;
    v["branch_snapshots"] = json!(repo.branch_snapshots.iter().map(|e| snapshot_json(repo, e)).collect::<Vec<_>>());
    v["releases"] = json!(repo.releases.iter().map(|e| snapshot_json(repo, e)).collect::<Vec<_>>());
    v
}

// ---------- handlers ----------

async fn list_repos(
    State(st): State<Arc<AppState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let tags: Vec<String> = q
        .get("tags")
        .map(|s| s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    let query = q.get("q").cloned();
    let folder = q.get("folder").cloned();

    let index = st.index.read().await;
    let mut repos = index.filter(&tags, query.as_deref());
    if let Some(f) = &folder {
        let f = f.trim_end_matches('/');
        repos.retain(|r| r.rel == f || r.rel.starts_with(&format!("{f}/")));
    }
    Ok(ok_json(json!({
        "repos": repos.iter().map(|r| repo_json(r, false)).collect::<Vec<_>>(),
        "total": repos.len(),
    })))
}

async fn get_repo_detail(
    st: &Arc<AppState>,
    rest: &str,
) -> Result<Response, ApiError> {
    let repo = st
        .find_repo(rest)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no such repo: {rest}")))?;
    Ok(ok_json(repo_json(&repo, true)))
}

async fn download_archive(
    st: &Arc<AppState>,
    rel: &str,
    zip_rel: &str,
) -> Result<Response, ApiError> {
    let repo = st
        .find_repo(rel)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no such repo: {rel}")))?;
    let path = repo.dir.join(zip_rel);
    let path = path.canonicalize().map_err(|_| ApiError::not_found("no such snapshot"))?;
    let repo_dir = repo
        .dir
        .canonicalize()
        .map_err(|e| ApiError::internal(anyhow::anyhow!("canonicalize failed: {e}")))?;
    if !path.starts_with(&repo_dir) {
        return Err(ApiError::not_found("no such snapshot"));
    }
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("snapshot")
        .to_string();
    // Both archive formats are downloadable. Detect by name because
    // `Path::extension` on "foo.tar.zst" only yields "zst".
    let content_type = if filename.ends_with(".tar.zst") {
        "application/zstd"
    } else if filename.ends_with(".zip") {
        "application/zip"
    } else {
        return Err(ApiError::bad_request(
            "only zip and tar.zst downloads are supported",
        ));
    };
    stream_attachment(&path, &filename, content_type).await
}

/// Stream a file from disk as an attachment. Shared by snapshot and release
/// asset downloads; the Content-Disposition filename is sanitized so a hostile
/// name cannot inject headers (`HeaderValue::from_str` also rejects controls).
async fn stream_attachment(
    path: &Path,
    filename: &str,
    content_type: &str,
) -> Result<Response, ApiError> {
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| ApiError::not_found("no such file"))?;
    let body = Body::from_stream(tokio_util::io::ReaderStream::new(file));
    let mut headers = HeaderMap::new();
    if let Ok(v) = header::HeaderValue::from_str(content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    let disposition = format!("attachment; filename=\"{}\"", filename.replace('"', ""));
    if let Ok(v) = header::HeaderValue::from_str(&disposition) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    Ok((headers, body).into_response())
}

/// Content type for a downloaded release asset, by filename. Everything
/// unknown is served as an opaque attachment.
fn asset_content_type(name: &str) -> &'static str {
    let n = name.to_ascii_lowercase();
    if n.ends_with(".zip") {
        "application/zip"
    } else if n.ends_with(".tar.gz") || n.ends_with(".tgz") {
        "application/gzip"
    } else if n.ends_with(".tar.zst") || n.ends_with(".zst") {
        "application/zstd"
    } else if n.ends_with(".tar.xz") || n.ends_with(".xz") {
        "application/x-xz"
    } else if n.ends_with(".tar.bz2") || n.ends_with(".tbz2") {
        "application/x-bzip2"
    } else if n.ends_with(".dmg") {
        "application/x-apple-diskimage"
    } else if n.ends_with(".deb") {
        "application/vnd.debian.binary-package"
    } else if n.ends_with(".rpm") {
        "application/x-rpm"
    } else if n.ends_with(".txt")
        || n.ends_with(".sha256")
        || n.ends_with(".sha512")
        || n.ends_with(".asc")
        || n.ends_with(".sig")
    {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// Stream a downloaded release asset from `<repo>/releases/<tag>/assets/`.
/// Paths are confined to the repo directory (traversal guard), exactly like
/// archive downloads.
async fn download_asset(
    st: &Arc<AppState>,
    rel: &str,
    asset_rel: &str,
) -> Result<Response, ApiError> {
    if asset_rel.is_empty() || asset_rel.split('/').any(|s| s == "..") {
        return Err(ApiError::bad_request("bad asset path"));
    }
    let repo = st
        .find_repo(rel)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no such repo: {rel}")))?;
    let path = repo
        .dir
        .join(asset_rel)
        .canonicalize()
        .map_err(|_| ApiError::not_found("no such asset"))?;
    let repo_dir = repo
        .dir
        .canonicalize()
        .map_err(|e| ApiError::internal(anyhow::anyhow!("canonicalize failed: {e}")))?;
    if !path.starts_with(&repo_dir) || !path.is_file() {
        return Err(ApiError::not_found("no such asset"));
    }
    // only serve files that actually sit in a release's assets/ directory
    let in_assets = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        == Some("assets");
    if !in_assets {
        return Err(ApiError::not_found("no such asset"));
    }
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("asset")
        .to_string();
    stream_attachment(&path, &filename, asset_content_type(&filename)).await
}

async fn repo_notifications(st: &Arc<AppState>, rel: &str) -> Result<Response, ApiError> {
    let ns = st.notifications.lock().await;
    let mine: Vec<&Notification> = ns.iter().filter(|n| n.repo == rel).collect();
    Ok(ok_json(json!({ "repo": rel, "notifications": mine })))
}

async fn repo_get(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
) -> Result<Response, ApiError> {
    if let Some(rel) = rest.strip_suffix("/notifications") {
        return repo_notifications(&st, rel).await;
    }
    // A repo path may itself contain segments named `archive`/`asset` (a
    // category folder, or a branch named `archive`), so match against the
    // known repo paths and take the longest prefix instead of splitting
    // blindly on the first `/archive/`.
    let sub = {
        let index = st.index.read().await;
        let mut best: Option<(usize, &'static str, String, String)> = None;
        for r in &index.repos {
            for (marker, kind) in [("/archive/", "archive"), ("/asset/", "asset")] {
                if let Some(tail) = rest.strip_prefix(&format!("{}{marker}", r.rel)) {
                    if best.as_ref().is_none_or(|(len, ..)| r.rel.len() > *len) {
                        best = Some((r.rel.len(), kind, r.rel.clone(), tail.to_string()));
                    }
                }
            }
        }
        best
    };
    match sub {
        Some((_, "archive", rel, tail)) => download_archive(&st, &rel, &tail).await,
        Some((_, "asset", rel, tail)) => download_asset(&st, &rel, &tail).await,
        _ => get_repo_detail(&st, &rest).await,
    }
}

/// Spawn a background add job (shared by the REST API and the web UI).
/// `folder` optionally parks the freshly archived repo under a category
/// folder (empty = archive root).
pub async fn spawn_add_job(
    st: &Arc<AppState>,
    url: &str,
    tags: &[String],
    notes: Option<String>,
    folder: Option<String>,
) -> u64 {
    let job_url = url.to_string();
    let id = st.create_job("add", Some(job_url.clone())).await;
    // Reserve the owner-repo slug so two concurrent adds of the same repo can
    // never race in the same directory (the same key `refresh` uses).
    let lock_key = crate::forge::detect(url).ok().map(|info| {
        format!(
            "{}-{}",
            crate::archiver::sanitize(&info.owner),
            crate::archiver::sanitize(&info.name)
        )
    });
    if let Some(key) = &lock_key {
        if !st.try_lock_repo(key).await {
            st.finish_job(id, Some(format!("a job is already running for {key}")))
                .await;
            return id;
        }
    }
    let st2 = st.clone();
    let url = job_url;
    let tags = tags.to_vec();
    let folder = folder.filter(|f| !f.trim().is_empty());
    tokio::spawn(async move {
        // RAII: release the slug lock even if the job panics or returns early.
        let _lock = lock_key.as_deref().map(|k| RepoLockGuard::new(st2.clone(), k));
        let result = Archiver::new(st2.cfg().await).add_repo(&url, &tags, notes).await;
        match result {
            Ok(dir) => {
                tracing::info!(repo = ?dir, "add finished");
                let _ = st2.reindex().await;
                if let Some(folder) = &folder {
                    let root = st2.root().await;
                    let rel = dir.strip_prefix(&root).unwrap_or(&dir).to_string_lossy().into_owned();
                    if let Some(repo) = st2.find_repo(&rel).await {
                        match move_repo_to_folder(&st2, &repo, folder).await {
                            Ok(_) => {
                                let _ = st2.reindex().await;
                            }
                            Err(e) => {
                                // the archive itself succeeded; only the placement failed
                                tracing::warn!(repo = %rel, "folder move failed: {e}");
                            }
                        }
                    }
                }
                st2.finish_job(id, None).await;
                st2.record(&["add_ok"]).await;
            }
            Err(e) => {
                tracing::error!(repo = %url, "add failed: {e:#}");
                st2.finish_job(id, Some(format!("{e:#}"))).await;
                st2.record(&["add_fail"]).await;
            }
        }
    });
    id
}

/// A single folder name component: no separators, no hidden/reserved names.
pub fn valid_folder_name(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty()
        && !n.contains('/')
        && !n.contains('\\')
        && n != "."
        && n != ".."
        && !n.starts_with('.')
        && n != "_unknown" // reserved for unidentified imports
        && n.len() <= 64
}

/// A repo-move target: every component is a legal folder name and the path
/// doesn't collide with (or run through) another repo's directory. The
/// folder does not need to exist yet; moving creates it implicitly.
pub fn valid_move_target(path: &str, index: &Index) -> bool {
    if path.is_empty() {
        return true;
    }
    if !path.split('/').all(valid_folder_name) {
        return false;
    }
    !index
        .repos
        .iter()
        .any(|r| r.rel == path || path.starts_with(&format!("{}/", r.rel)))
}

/// Move a repo into a folder (empty = archive root), returning its new rel.
/// Callers hold the repo lock and reindex afterwards.
pub async fn move_repo_to_folder(
    st: &Arc<AppState>,
    repo: &RepoEntry,
    dest_folder: &str,
) -> Result<String, String> {
    let dest_folder = dest_folder.trim().trim_matches('/');
    let current_parent = repo.rel.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    if dest_folder == current_parent {
        return Ok(repo.rel.clone());
    }
    if !dest_folder.is_empty() {
        let ok = { let index = st.index.read().await; valid_move_target(dest_folder, &index) };
        if !ok {
            return Err(format!("invalid folder path: {dest_folder}"));
        }
    }
    let root = st.root().await;
    let repo_name = repo.rel.rsplit('/').next().unwrap_or(&repo.rel).to_string();
    let dest = if dest_folder.is_empty() {
        root.join(&repo_name)
    } else {
        root.join(dest_folder).join(&repo_name)
    };
    if dest == repo.dir {
        return Ok(repo.rel.clone());
    }
    if dest.exists() {
        return Err(format!("cannot move: {} already exists", dest.display()));
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("cannot create folder: {e}"))?;
    }
    tokio::fs::rename(&repo.dir, &dest)
        .await
        .map_err(|e| format!("cannot move repo: {e}"))?;
    Ok(dest.strip_prefix(&root).unwrap_or(&dest).to_string_lossy().into_owned())
}

async fn create_repo(
    State(st): State<Arc<AppState>>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let url = body["url"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let Some(url) = url else {
        return Err(ApiError::bad_request("body must include { \"url\": \"...\" }"));
    };
    let tags: Vec<String> = body["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let notes = body["notes"].as_str().map(String::from);
    let folder = body["folder"].as_str().map(String::from);

    let id = spawn_add_job(&st, url, &tags, notes, folder).await;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": id, "url": url }))).into_response())
}

/// POST /api/repos/{repo}/refresh
async fn refresh_repo(
    st: &Arc<AppState>,
    rest: &str,
) -> Result<Response, ApiError> {
    let Some(rel) = rest.strip_suffix("/refresh") else {
        return Err(ApiError::not_found("unknown POST action; try .../refresh"));
    };
    st.find_repo(rel).await.ok_or_else(|| ApiError::not_found(format!("no such repo: {rel}")))?;
    if !st.try_lock_repo(rel).await {
        return Err(ApiError::conflict("a job is already running for this repo"));
    }
    let id = spawn_refresh_job(st.clone(), rel.to_string(), "manual-refresh", None).await;
    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": id, "repo": rel }))).into_response())
}

async fn repo_post(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
) -> Result<Response, ApiError> {
    refresh_repo(&st, &rest).await
}

/// Shared background job for manual + scheduled refreshes.
/// Caller must have locked the repo via try_lock_repo; the job unlocks when
/// it finishes. An optional semaphore permit bounds scheduler concurrency.
pub async fn spawn_refresh_job(
    st: Arc<AppState>,
    rel: String,
    kind: &str,
    permit: Option<OwnedSemaphorePermit>,
) -> u64 {
    let id = st.create_job(kind, Some(rel.clone())).await;
    let _ = permit; // moved into the task below to bound concurrency
    tokio::spawn(async move {
        // Caller already locked the repo; this releases it on completion or panic.
        let _lock = RepoLockGuard::new(st.clone(), &rel);
        let _permit = permit;
        let dir = { st.index.read().await.find(&rel).cloned().map(|r| r.dir) };
        let result = match dir {
            Some(dir) => Archiver::new(st.cfg().await).refresh_repo(&dir).await,
            None => Err(anyhow::anyhow!("repo disappeared from index")),
        };
        match result {
            Ok(summary) => {
                let mut fields: Vec<&str> = vec!["refresh_ok"];
                if let Some(v) = &summary.new_release {
                    st.push_notification("new_release", &rel, format!("new release {v}"), None).await;
                    fields.push("new_releases");
                }
                if summary.new_branch_snapshot {
                    st.push_notification("new_snapshot", &rel, "new branch snapshot archived".to_string(), None).await;
                    fields.push("new_snapshots");
                }
                if summary.remote_unavailable {
                    st.push_notification("remote_gone", &rel, "remote unreachable; local copy intact".to_string(), None).await;
                    fields.push("remote_gone");
                }
                let _ = st.reindex().await;
                st.finish_job(id, None).await;
                st.record(&fields).await;
            }
            Err(e) => {
                tracing::error!(repo = %rel, "refresh failed: {e:#}");
                st.finish_job(id, Some(format!("{e:#}"))).await;
                st.record(&["refresh_fail"]).await;
            }
        }
    });
    id
}

/// Assign/replace a repo's origin: validates the URL, relocates the repo
/// folder to its owner-repo slug (promoting _unknown/ entries), updates the
/// manifest, and reindexes. Returns the (possibly new) rel path.
pub async fn assign_origin(st: &Arc<AppState>, repo: &RepoEntry, origin: &str) -> Result<String, ApiError> {
    let origin = origin.trim();
    let info = crate::forge::detect(origin).map_err(|e| ApiError::bad_request(format!("invalid origin: {e:#}")))?;
    let mut manifest = repo.manifest.clone();

    // promote/relocate to the flat owner-repo slug
    let root = st.root().await;
    let new_slug = format!("{}-{}", crate::archiver::sanitize(&info.owner), crate::archiver::sanitize(&info.name));
    let old_name = repo.dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    let mut new_rel = repo.rel.clone();
    let mut new_dir = repo.dir.clone();
    if old_name != new_slug {
        // preserve any category folders the repo sits under (except _unknown)
        let parent = repo.dir.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| root.clone());
        let parent_name = repo.dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or_default();
        let dest_parent = if parent_name == "_unknown" { root.clone() } else { parent };
        let dest = dest_parent.join(&new_slug);
        if dest.exists() {
            return Err(ApiError::conflict(format!(
                "cannot move to {}: target already exists (is this repo already archived?)",
                dest.display()
            )));
        }
        tokio::fs::rename(&repo.dir, &dest)
            .await
            .map_err(|e| ApiError::internal(anyhow::anyhow!("cannot move repo folder: {e}")))?;
        new_dir = dest;
        new_rel = new_dir
            .strip_prefix(&root)
            .unwrap_or(&new_dir)
            .to_string_lossy()
            .into_owned();
    }

    manifest.origin = Some(origin.to_string());
    manifest.forge = info.kind.id().to_string();
    manifest.name = info.name.clone();
    manifest.unidentified = false;
    manifest.remote_state = None;
    manifest.unavailable_since = None;
    manifest.tags.retain(|t| t != "unidentified");
    crate::types::write_json(&new_dir.join("repo.json"), &manifest)
        .map_err(|e| ApiError::internal(anyhow::anyhow!("{e}")))?;
    st.reindex().await.map_err(ApiError::internal)?;
    Ok(new_rel)
}

/// PATCH /api/repos/{rel}: edit tags, notes, description, schedule, retention, origin.
async fn repo_patch(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    let repo = st
        .find_repo(&rest)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no such repo: {rest}")))?;
    if let Some(o) = body["origin"].as_str().filter(|s| !s.trim().is_empty()) {
        if !st.try_lock_repo(&repo.rel).await {
            return Err(ApiError::conflict("a job is running for this repo"));
        }
        let result = assign_origin(&st, &repo, o).await;
        st.unlock_repo(&repo.rel).await;
        let new_rel = result?;
        let updated = st
            .find_repo(&new_rel)
            .await
            .ok_or_else(|| ApiError::internal(anyhow::anyhow!("repo vanished after move")))?;
        let mut resp = repo_json(&updated, false);
        resp["moved_to"] = json!(new_rel);
        return Ok(ok_json(resp));
    }
    let mut manifest = repo.manifest.clone();

    if let Some(tags) = body["tags"].as_array() {
        manifest.tags = tags.iter().filter_map(|t| t.as_str().map(String::from)).collect();
    }
    if let Some(n) = body["notes"].as_str() {
        manifest.notes = Some(n.to_string());
    }
    if let Some(d) = body["description"].as_str() {
        manifest.description = Some(d.to_string());
    }
    if let Some(n) = body["name"].as_str().filter(|s| !s.trim().is_empty()) {
        manifest.name = n.trim().to_string();
    }
    if let Some(days) = body["schedule"]["interval_days"].as_u64() {
        manifest.schedule.interval_days = days as u32;
    }
    if let Some(k) = body["retention"]["keep_branch_snapshots"].as_i64() {
        manifest.retention.keep_branch_snapshots = Some(k);
    }
    if let Some(k) = body["retention"]["keep_releases"].as_i64() {
        manifest.retention.keep_releases = Some(k);
    }
    if body.as_object().is_some_and(|o| o.is_empty()) {
        return Err(ApiError::bad_request("no editable fields provided (tags, notes, description, schedule, retention)"));
    }
    write_json(&repo.dir.join("repo.json"), &manifest).map_err(ApiError::internal)?;
    st.reindex().await.map_err(ApiError::internal)?;
    let updated = st.find_repo(&rest).await.ok_or_else(|| ApiError::internal(anyhow::anyhow!("repo vanished after update")))?;
    Ok(ok_json(repo_json(&updated, false)))
}

/// Delete (unregister and optionally remove files) a repo. Shared by the API
/// and the web UI. Returns the API error shape on failure.
pub async fn delete_repo(st: &Arc<AppState>, repo: &RepoEntry, delete_files: bool) -> Result<(), ApiError> {
    if !st.try_lock_repo(&repo.rel).await {
        return Err(ApiError::conflict("a job is running for this repo"));
    }
    let result = if delete_files {
        tokio::fs::remove_dir_all(&repo.dir).await
    } else {
        tokio::fs::remove_file(repo.dir.join("repo.json")).await
    };
    st.unlock_repo(&repo.rel).await;
    result.map_err(|e| ApiError::internal(anyhow::anyhow!("delete failed: {e}")))?;
    st.reindex().await.map_err(ApiError::internal)?;
    Ok(())
}

/// DELETE /api/repos/{rel}?files=true|false (default false)
async fn repo_delete(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let repo = st
        .find_repo(&rest)
        .await
        .ok_or_else(|| ApiError::not_found(format!("no such repo: {rest}")))?;
    let delete_files = q
        .get("files")
        .map(|s| s == "true" || s == "1")
        .unwrap_or(false);
    delete_repo(&st, &repo, delete_files).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn list_tags(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let index = st.index.read().await;
    let tags: Vec<Value> = index
        .all_tags()
        .into_iter()
        .map(|(tag, count)| json!({ "tag": tag, "count": count }))
        .collect();
    Ok(ok_json(json!({ "tags": tags })))
}

#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    repos: Vec<String>,
}

fn tree_to_json(path: &str, name: &str, node: &TreeNode) -> Value {
    let children: Vec<Value> = node
        .children
        .iter()
        .map(|(child_name, child)| tree_to_json(&format!("{path}/{child_name}"), child_name, child))
        .collect();
    let total_repos: usize = node
        .children
        .values()
        .map(|c| c.repos.len())
        .sum::<usize>()
        + node.repos.len();
    json!({
        "name": name,
        "path": path,
        "repos": node.repos,
        "children": children,
        "repo_count": total_repos,
    })
}

async fn tree(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let index = st.index.read().await;
    let mut root = TreeNode::default();
    for r in &index.repos {
        let mut node = &mut root;
        let parts: Vec<&str> = r.rel.split('/').collect();
        let folder_parts = &parts[..parts.len() - 1];
        for p in folder_parts {
            node = node.children.entry(p.to_string()).or_default();
        }
        node.repos.push(r.rel.clone());
    }
    Ok(ok_json(tree_to_json("", "", &root)))
}

async fn list_jobs(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let jobs = st.jobs.lock().await;
    let mut list: Vec<&Job> = jobs.values().collect();
    list.sort_by_key(|j| std::cmp::Reverse(j.id));
    list.truncate(100);
    Ok(ok_json(json!({ "jobs": list })))
}

async fn list_notifications(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let ns = st.notifications.lock().await;
    let list: Vec<Notification> = ns.iter().take(100).cloned().collect();
    Ok(ok_json(json!({ "notifications": list })))
}

async fn autotag_trigger(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let cfg = st.cfg().await;
    if !cfg.llm.enabled {
        return Err(ApiError::bad_request("llama.cpp not enabled in config"));
    }
    let repos: Vec<RepoEntry> = st
        .index
        .read()
        .await
        .repos
        .iter()
        .filter(|r| r.manifest.tags.is_empty())
        .cloned()
        .collect();
    let n = repos.len();
    if n == 0 {
        return Ok(ok_json(json!({ "message": "no untagged repos" })));
    }
    let id = st.create_job("autotag", None).await;
    let st2 = st.clone();
    tokio::spawn(async move {
        let cfg = st2.cfg().await;
        let report = crate::tagging::run_autotag(&cfg, &repos, false).await;
        let _ = st2.reindex().await;
        st2.push_notification("autotag", "", format!("AI tagged {} repos", report.changed), None).await;
        st2.finish_job(id, None).await;
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "job_id": id, "repos": n }))).into_response())
}

async fn bell_count(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Result<Response, ApiError> {
    let user = crate::auth::current_user(&st, &headers);
    let count = match user.as_deref().and_then(|u| st.users.lock().ok().and_then(|mut users| users.find(u).and_then(|user| user.last_seen))) {
        Some(seen) => st.notification_count(Some(&seen)).await,
        None => st.notification_count(None).await,
    };
    // plain text for htmx innerHTML swap: just the number, empty when 0
    let body = if count > 0 { count.to_string() } else { String::new() };
    Ok(([(
        header::CONTENT_TYPE,
        "text/plain; charset=utf-8",
    )], body)
        .into_response())
}

async fn reindex_route(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    st.reindex().await.map_err(ApiError::internal)?;
    let index = st.index.read().await;
    Ok(ok_json(json!({
        "repos": index.repos.len(),
        "snapshots": index.snapshot_count(),
        "tags": index.all_tags().len(),
    })))
}

/// GET /api/stats: totals + the last 30 days of counters.
async fn stats(State(st): State<Arc<AppState>>) -> Result<Response, ApiError> {
    let (m, t) = st.stats_snapshot().await;
    let sums = m.sums();
    let mut totals = serde_json::to_value(&t).unwrap_or_default();
    if let (Some(obj), Ok(serde_json::Value::Object(sobj))) =
        (totals.as_object_mut(), serde_json::to_value(&sums))
    {
        for (k, v) in sobj {
            obj.insert(k, v);
        }
    }
    let days: Vec<Value> = m
        .days
        .iter()
        .rev()
        .take(30)
        .rev()
        .map(|(date, s)| json!({ "date": date, "stats": s }))
        .collect();
    Ok(ok_json(json!({
        "started_at": m.started_at,
        "totals": totals,
        "days": days,
    })))
}

// ---------- scheduler ----------

fn hours_since_rfc3339(s: &str) -> Option<f64> {
    let t = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    let d = OffsetDateTime::now_utc() - t;
    Some(d.whole_nanoseconds() as f64 / 3_600_000_000_000.0)
}

async fn compute_due(st: &Arc<AppState>) -> Vec<String> {
    // read config first, then the index: never hold one async lock across another
    let poll_h = st.cfg().await.scheduler.release_poll_hours as f64;
    let index = st.index.read().await;
    index
        .repos
        .iter()
        .filter_map(|r| {
            // unidentified imports have no origin; nothing to schedule
            r.manifest.origin.as_ref()?;
            // dead remotes: we already know; stop reaching out (manual refresh
            // can still resurrect them)
            if r.manifest.remote_state.as_deref() == Some("dead") {
                return None;
            }
            let branch_h = r.manifest.schedule.interval_days as f64 * 24.0;
            let due_h = branch_h.min(poll_h);
            match r.manifest.last_checked.as_deref().and_then(hours_since_rfc3339) {
                Some(h) if h >= due_h => Some(r.rel.clone()),
                None => Some(r.rel.clone()),
                _ => None,
            }
        })
        .collect()
}

async fn scheduler_loop(st: Arc<AppState>) {
    let poll_secs = st.cfg().await.scheduler.poll_every_secs.max(60);
    let sem = Arc::new(Semaphore::new(st.cfg().await.scheduler.max_concurrent.max(1)));
    tracing::info!(poll_every_secs = poll_secs, "scheduler started");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(poll_secs as u64)).await;
        let due = compute_due(&st).await;
        if due.is_empty() {
            continue;
        }
        tracing::info!(count = due.len(), "scheduler: refreshing due repos");
        for rel in due {
            if !st.try_lock_repo(&rel).await {
                continue; // already running
            }
            // A scheduled refresh waits for a permit before even starting;
            // the permit is held for the whole job, bounding concurrency.
            let permit = sem.clone().acquire_owned().await.unwrap();
            spawn_refresh_job(st.clone(), rel, "scheduled-refresh", Some(permit)).await;
        }
    }
}

/// Periodically push a metrics snapshot to OTLP when enabled. Always running
/// so the setting can be toggled live; a no-op (just a timer) when disabled.
async fn otel_loop(st: Arc<AppState>) {
    loop {
        let cfg = st.cfg().await;
        if cfg.otel.enabled {
            let (metrics, totals) = st.stats_snapshot().await;
            if let Err(e) = crate::metrics::export_otlp(
                &cfg.otel.endpoint,
                &cfg.otel.service_name,
                &metrics,
                &totals,
            )
            .await
            {
                tracing::warn!(error = %e, "OTLP export failed");
            }
        }
        let secs = if cfg.otel.enabled {
            cfg.otel.interval_secs.max(10)
        } else {
            60
        };
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }
}

// ---------- router & serve ----------

pub fn router(st: Arc<AppState>) -> Router {
    // auth middleware runs on everything except /login, /logout, /assets
    let st2 = st.clone();
    let auth_layer = axum::middleware::from_fn_with_state(st2, crate::auth::mw);
    Router::new()
        .route(
            "/api/repos",
            get(list_repos).post(create_repo),
        )
        .route(
            "/api/repos/{*rest}",
            get(repo_get).patch(repo_patch).delete(repo_delete).post(repo_post),
        )
        .route("/api/tags", get(list_tags))
        .route("/api/tree", get(tree))
        .route("/api/jobs", get(list_jobs))
        .route("/api/notifications", get(list_notifications))
        .route("/api/reindex", post(reindex_route))
        .route("/api/stats", get(stats))
        .route("/api/bell", get(bell_count))
        .route("/api/autotag", post(autotag_trigger))
        .route("/login", axum::routing::get(crate::auth::login_page).post(crate::auth::login_submit))
        .route("/logout", axum::routing::post(crate::auth::logout))
        .merge(crate::web::router())
        .layer(auth_layer)
        .with_state(st)
}

pub async fn serve(cfg: Config, config_path: Option<PathBuf>, no_scheduler: bool) -> Result<()> {
    let st = Arc::new(AppState::new(cfg, config_path).await?);
    if !no_scheduler && st.cfg().await.scheduler.enabled {
        tokio::spawn(scheduler_loop(st.clone()));
    } else {
        tracing::info!("scheduler disabled");
    }
    tokio::spawn(otel_loop(st.clone()));
    let bind = st.cfg().await.server.bind.clone();
    let app = router(st);
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("cannot bind {bind}"))?;
    println!("reposilo serving http://{bind} (API: /api/repos, /api/tags, /api/tree, /api/jobs)");
    axum::serve(listener, app).await.context("server crashed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RepoManifest;

    fn manifest(origin: Option<&str>, last_checked: Option<String>, remote_state: Option<&str>) -> RepoManifest {
        RepoManifest {
            origin: origin.map(str::to_string),
            forge: "generic".into(),
            name: "x".into(),
            added: "2025-01-01T00:00:00Z".into(),
            tags: vec![],
            description: None,
            language: None,
            default_branch: "master".into(),
            schedule: Default::default(),
            retention: Default::default(),
            last_checked,
            notes: None,
            remote_state: remote_state.map(str::to_string),
            unavailable_since: None,
            suggested_tags: vec![],
            stars: None,
            unidentified: false,
        }
    }

    fn rfc_days_ago(days: i64) -> String {
        (OffsetDateTime::now_utc() - time::Duration::days(days))
            .format(&Rfc3339)
            .unwrap()
    }

    /// The scheduler's "who do we call out to?" decision: only live repos with
    /// an origin whose check is older than the interval (or that were never
    /// checked). Dead / unidentified / fresh repos must be skipped.
    #[tokio::test]
    async fn scheduler_due_selection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cases = [
            ("fresh", manifest(Some("file:///r/fresh"), Some(rfc_days_ago(0)), None)),
            ("stale", manifest(Some("file:///r/stale"), Some(rfc_days_ago(30)), None)),
            ("dead", manifest(Some("file:///r/dead"), Some(rfc_days_ago(30)), Some("dead"))),
            ("imported", manifest(None, Some(rfc_days_ago(30)), None)),
            ("never", manifest(Some("file:///r/never"), None, None)),
        ];
        for (name, m) in &cases {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            crate::types::write_json(&dir.join("repo.json"), m).unwrap();
        }
        let mut cfg = Config::default();
        cfg.archive.root = root.to_string_lossy().into_owned();
        cfg.scheduler.release_poll_hours = 24;
        let st = Arc::new(AppState::new(cfg, None).await.unwrap());
        let mut due = compute_due(&st).await;
        due.sort();
        assert_eq!(due, vec!["never".to_string(), "stale".to_string()]);
    }

    /// Locks must be keyed by basename so a full relative path (refresh) and a
    /// bare slug (add) collide instead of racing in the same directory.
    #[tokio::test]
    async fn lock_keys_are_normalized_to_basename() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        let st = AppState::new(cfg, None).await.unwrap();

        assert!(st.try_lock_repo("category/remotes-dup").await);
        assert!(
            !st.try_lock_repo("remotes-dup").await,
            "the bare slug must see the path-keyed lock"
        );
        st.unlock_repo("remotes-dup").await;
        assert!(
            st.try_lock_repo("category/remotes-dup").await,
            "unlocking by basename releases the path-keyed lock"
        );
        st.unlock_repo("category/remotes-dup").await;
    }

    /// Job history must stay bounded for a process that runs for months.
    #[tokio::test]
    async fn job_history_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        let st = AppState::new(cfg, None).await.unwrap();
        for _ in 0..(MAX_TRACKED_JOBS + 137) {
            st.create_job("scheduled-refresh", None).await;
        }
        assert!(st.jobs.lock().await.len() <= MAX_TRACKED_JOBS);
    }

    /// The RAII guard must release the repo lock even on an early exit, so a
    /// panicking job can never leave a repo locked forever.
    #[tokio::test]
    async fn repo_lock_guard_releases_on_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        let st = Arc::new(AppState::new(cfg, None).await.unwrap());
        assert!(st.try_lock_repo("owner-repo").await);
        {
            let _g = RepoLockGuard::new(st.clone(), "owner-repo");
            assert!(!st.try_lock_repo("owner-repo").await, "held while guard alive");
        }
        assert!(st.try_lock_repo("owner-repo").await, "released after guard drop");
    }

    /// The archive listing cache must return the same parsed index for repeat
    /// lookups (this is what makes tar.zst browsing cheap).
    #[tokio::test]
    async fn archive_listing_is_cached() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.archive.root = tmp.path().to_string_lossy().into_owned();
        let st = AppState::new(cfg, None).await.unwrap();

        let zpath = tmp.path().join("x.zip");
        {
            let f = std::fs::File::create(&zpath).unwrap();
            let mut z = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            z.start_file("p/README.md", opts).unwrap();
            std::io::Write::write_all(&mut z, b"# hi").unwrap();
            z.finish().unwrap();
        }
        let a = st.archive_index(&zpath).await.unwrap();
        let b = st.archive_index(&zpath).await.unwrap();
        assert!(Arc::ptr_eq(&a, &b), "second lookup must hit the cache");
        assert!(a.find("README.md").is_some());
    }
}