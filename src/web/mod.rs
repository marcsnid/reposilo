//! HTMX web UI routes. All interactivity is server-rendered HTML fragments
//! swapped by the vendored htmx.min.js; no build pipeline, no node.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::body::Body;
use axum::extract::{Form, Path as AxPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::server::AppState;

pub mod views;
use views::{
    build_detail_ctx, build_list_ctx, page_url, BlobCtx, BlobT, DetailT, FolderFormCtx, FolderFormT,
    FragmentT, IndexT, JobCtx, JobT, TagsCtx, TagsT,
};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/", get(index_page))
        .route("/healthz", get(healthz))
        .route("/notifications", get(notifications_page).post(notifications_mark_read))
        .route("/stats", get(stats_page))
        .route("/import", get(import_page))
        .route("/settings", get(settings_page).post(settings_save))
        .route("/settings/verify", post(settings_verify))
        .route("/settings/verify/{id}", get(settings_verify_status))
        .route("/import/scan", post(import_scan))
        .route("/import/suggest", get(import_suggest))
        .route("/import/llm", post(import_llm))
        .route("/import/commit", post(import_commit))
        .route("/repos/add", post(add_repo_form))
        .route("/repos/job-status/{id}", get(job_status))
        .route("/folders/new", get(folders_new))
        .route("/folders/close", get(folders_close))
        .route("/folders/edit", get(folders_edit_form))
        .route("/folders/create", post(folders_create))
        .route("/folders/update", post(folders_update))
        .route("/folders/options", get(folders_options))
        .route("/repos/move", post(repo_move))
        .route("/repos/bulk", post(bulk_action))
        .route("/repos/bulk/tag", get(bulk_tag_modal))
        .route("/repos/bulk/move", get(bulk_move_modal))
        .route(
            "/repos/{*rest}",
            get(repo_page).post(repo_post),
        )
        .route("/repo-icon/{*rel}", get(repo_icon))
        .route("/assets/style.css", get(style_css))
        .route("/assets/htmx.min.js", get(htmx_js))
}

fn html(s: String) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], Body::from(s)).into_response()
}

/// Build the out-of-bounds app refresh (swaps #app in the grid after a job
/// completes, so new repos/tags appear without a page reload).
async fn oob_app_refresh(st: &Arc<AppState>) -> String {
    let ctx = views::build_list_ctx(st, &[], "", "").await;
    let inner = views::FragmentT { ctx }.render().unwrap_or_default();
    format!(r#"<div id="app" class="app" hx-swap-oob="outerHTML">{inner}</div>"#)
}

/// Out-of-band success toast: drop a status message into the fixed #toast
/// area while the main response swaps somewhere else.
fn oob_toast(msg: &str) -> String {
    format!(
        r#"<div id="toast" hx-swap-oob="innerHTML"><div class="job-status ok">{}</div></div>"#,
        html_escape(msg)
    )
}

/// Out-of-band close for a modal (e.g. after a successful form submit).
fn oob_close_modal(id: &str) -> String {
    format!(r#"<div id="{id}" hx-swap-oob="delete"></div>"#)
}

/// Keep only the first visible character of an icon so a pasted word cannot
/// break the sidebar layout. Emoji are grapheme clusters of several code
/// points, so variation selectors, ZWJ sequences, keycaps and skin-tone
/// modifiers that belong to the first character are kept with it.
fn icon_first_char(icon: &str) -> String {
    let mut chars = icon.chars();
    let Some(first) = chars.next() else { return String::new() };
    let mut out = String::new();
    out.push(first);
    let mut rest = chars.peekable();
    while let Some(&c) = rest.peek() {
        let joins = matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{20E3}' | '\u{200D}')
            || ('\u{1F3FB}'..='\u{1F3FF}').contains(&c)
            || matches!(c as u32, 0x0300..=0x036F | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF | 0x20D0..=0x20FF | 0xFE20..=0xFE2F);
        if !joins {
            break;
        }
        out.push(rest.next().unwrap());
        if c == '\u{200D}' {
            if let Some(next) = rest.next() {
                out.push(next);
            }
        }
    }
    out
}

fn render<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(s) => html(s),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template error: {e}"),
        )
            .into_response(),
    }
}

async fn style_css() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        Body::from(include_str!("../assets/style.css")),
    )
        .into_response()
}

async fn htmx_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
        Body::from(include_str!("../assets/htmx.min.js")),
    )
        .into_response()
}

/// GET /repo-icon/{rel}: serve a repo's stored owner avatar (if any).
async fn repo_icon(State(st): State<Arc<AppState>>, AxPath(rel): AxPath<String>) -> Response {
    let Some(repo) = st.find_repo(rel.trim_matches('/')).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    match tokio::fs::read(repo.dir.join("icon")).await {
        Ok(bytes) if !bytes.is_empty() => (
            [
                (header::CONTENT_TYPE, sniff_image(&bytes)),
                (header::CACHE_CONTROL, "public, max-age=86400"),
            ],
            bytes,
        )
            .into_response(),
        _ => (StatusCode::NOT_FOUND, "no icon").into_response(),
    }
}

/// Best-effort image content type from magic bytes (avatars are stored raw).
fn sniff_image(b: &[u8]) -> &'static str {
    if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if b.starts_with(b"GIF8") {
        "image/gif"
    } else if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else {
        "application/octet-stream"
    }
}

/// GET /healthz: unauthenticated liveness probe for containers and monitoring.
async fn healthz() -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        Body::from("ok"),
    )
        .into_response()
}

fn csv_param(map: &HashMap<String, String>, key: &str) -> Vec<String> {
    map.get(key)
        .map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// GET /: full page, or the #app fragment when htmx requests it (so the
/// same URL works for both navigation and fragment swaps, reload included).
async fn index_page(
    State(st): State<Arc<AppState>>,
    Query(map): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let tags = csv_param(&map, "tags");
    let q = map.get("q").cloned().unwrap_or_default();
    let folder = map.get("folder").cloned().unwrap_or_default();
    let ctx = build_list_ctx(&st, &tags, &q, &folder).await;
    if headers.contains_key("hx-request") {
        render(&FragmentT { ctx })
    } else {
        render(&IndexT { ctx })
    }
}

/// POST /repos/add: form-encoded { url, tags?, notes? }. Kicks off the same
/// background job the API uses, then returns a self-polling status element.
async fn add_repo_form(
    State(st): State<Arc<AppState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let url = form.get("url").map(|s| s.trim().to_string()).unwrap_or_default();
    if url.is_empty() {
        return html("<div class=\"job-status failed\">URL is required</div>".into());
    }
    let tags: Vec<String> = csv_param(&form, "tags");
    let notes = form.get("notes").map(|s| s.to_string()).filter(|s| !s.is_empty());
    let folder = form.get("folder").map(|s| s.to_string()).filter(|s| !s.trim().is_empty());
    let id = crate::server::spawn_add_job(&st, &url, &tags, notes, folder).await;
    let ctx = JobCtx {
        id,
        message: format!("Archiving {url}…"),
        done: false,
        failed: false,
        view_url: String::new(),
        oob_app: String::new(),
    };
    render(&JobT { ctx })
}

#[derive(Deserialize)]
struct JobIdPath {
    id: u64,
}

/// GET /repos/job-status/{id}: polling endpoint; when the job finishes it
/// out-of-bounds swaps the whole #app so the grid reflects the new state.
async fn job_status(State(st): State<Arc<AppState>>, AxPath(p): AxPath<JobIdPath>) -> Response {
    let Some(job) = st.job(p.id).await else {
        return html("<div class=\"job-status failed\">unknown job</div>".into());
    };
    let done = job.status != "running";
    let failed = job.status == "failed";

    // finished jobs with a recorded note (e.g. import summaries) use it as the message
    let note = if job.status != "running" { st.job_note(p.id).await } else { None };
    // for completed add jobs, find where it landed so we can link to it
    let mut view_url = String::new();
    let mut message = match (&job.kind[..], &job.status[..]) {
        ("add", "done") => "Archived!".to_string(),
        ("add", "failed") => format!("Failed: {}", job.error.clone().unwrap_or_default()),
        (_, "done") => "Refreshed".to_string(),
        (_, "failed") => format!("Failed: {}", job.error.clone().unwrap_or_default()),
        _ => "Working…".to_string(),
    };
    if let Some(n) = note {
        message = n;
    }
    if job.kind == "add" && done && !failed {
        if let Some(repo) = &job.repo {
            let index = st.index.read().await;
            if let Some(r) = index.repos.iter().find(|r| r.manifest.origin.as_deref() == Some(repo.as_str())) {
                view_url = format!("/repos/{}", r.rel);
                message = format!("Archived {}!", r.rel);
            }
        }
    }

    let mut oob_app = String::new();
    if done && !failed {
        oob_app = oob_app_refresh(&st).await;
    }
    let ctx = JobCtx { id: p.id, message, done, failed, view_url, oob_app };
    render(&JobT { ctx })
}

/// GET /repos/{rel} (detail) or /repos/{rel}/blob/{commit}/{file} (viewer).
async fn repo_page(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
) -> Response {
    if let Some((rel, blob)) = rest.split_once("/blob/") {
        return blob_page(&st, rel, blob).await;
    }
    if let Some(rel) = rest.strip_suffix("/delete") {
        return delete_confirm_ui(&st, rel).await;
    }
    let repo = match st.find_repo(&rest).await {
        Some(r) => r,
        None => {
            return (
                StatusCode::NOT_FOUND,
                format!("<!DOCTYPE html><html><body style='font-family:sans-serif;padding:2rem'><h1>404</h1><p>No such repo: {rest}</p><p><a href='/'>← back</a></p></body></html>"),
            )
                .into_response()
        }
    };
    let ctx = build_detail_ctx(&st, &repo).await;
    render(&DetailT { ctx })
}

async fn blob_page(st: &Arc<AppState>, rel: &str, blob: &str) -> Response {
    // blob = "{snapshot}/{file}"; `snapshot` is the zip file name (new URLs)
    // or a 40-char commit sha (legacy URLs). `file` is root-level only.
    let Some((snapshot, file)) = blob.split_once('/') else {
        return (StatusCode::BAD_REQUEST, "bad blob url").into_response();
    };
    if file.contains('/') || file.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad blob url").into_response();
    }
    let repo = match st.find_repo(rel).await {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, "no such repo").into_response(),
    };
    let is_legacy_commit = snapshot.len() == 40 && snapshot.bytes().all(|b| b.is_ascii_hexdigit());
    // find the snapshot entry holding this zip (or this commit, for legacy URLs)
    let entry = repo
        .branch_snapshots
        .iter()
        .chain(repo.releases.iter())
        .find(|e| {
            if is_legacy_commit {
                e.sidecar.commit == snapshot
            } else {
                e.sidecar.zip.file == snapshot
            }
        });
    let Some(entry) = entry else {
        return (StatusCode::NOT_FOUND, "no such snapshot").into_response();
    };
    let zip_path = entry.dir.join(&entry.sidecar.zip.file);
    // locate the archive entry via the cached listing; only names present in
    // the listing are reachable (traversal guard), and both formats work
    let Some(idx) = st.archive_index(&zip_path).await else {
        return (StatusCode::NOT_FOUND, "no such snapshot").into_response();
    };
    let Some(archive_entry) = idx.find(file) else {
        return (StatusCode::NOT_FOUND, "no such file").into_response();
    };
    const MAX: usize = 512 * 1024;
    let bytes = match crate::files::read_archive(&zip_path, &archive_entry, MAX) {
        Some(b) => b,
        None => return (StatusCode::NOT_FOUND, "cannot read file").into_response(),
    };
    let truncated = bytes.len() >= MAX;

    let is_markdown = file.to_lowercase().ends_with(".md") || file.to_lowercase().ends_with(".markdown");
    let is_text = is_markdown || !bytes.iter().take(8192).any(|&b| b == 0);
    if !is_text {
        return (StatusCode::UNSUPPORTED_MEDIA_TYPE, "binary file").into_response();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let content = if is_markdown {
        crate::files::markdown_to_html(&text)
    } else {
        // plain text: blob.html renders it through askama's auto-escaping
        text
    };
    let ctx = BlobCtx {
        rel: rel.to_string(),
        name: file.to_string(),
        commit: snapshot.to_string(),
        is_markdown,
        content,
        truncated,
        back_url: format!("/repos/{rel}"),
    };
    render(&BlobT { ctx })
}

#[derive(Deserialize, Default)]
struct TagEditForm {
    #[serde(default)] op: String,    // "add" | "remove"
    #[serde(default)] value: String, // tag to add/remove
}

/// POST /repos/{rel}/tags: add or remove a tag, returns the editor fragment.
async fn update_tags(
    st: &Arc<AppState>,
    rel: &str,
    form: TagEditForm,
) -> Response {
    let repo = match st.find_repo(rel).await {
        Some(r) => r,
        None => return (StatusCode::NOT_FOUND, "no such repo").into_response(),
    };
    // The manifest on disk is the source of truth; the form posts only the edit
    // to apply (no client-side tag list that could be stale).
    let manifest_path = repo.dir.join("repo.json");
    let mut manifest = match crate::types::read_json::<crate::types::RepoManifest>(&manifest_path) {
        Ok(m) => m,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response(),
    };
    let mut tags = manifest.tags.clone();
    match form.op.as_str() {
        "add" => {
            let v = form.value.trim().to_string();
            if v.is_empty() {
                return (StatusCode::BAD_REQUEST, "empty tag").into_response();
            }
            if !tags.iter().any(|t| t.eq_ignore_ascii_case(&v)) {
                tags.push(v);
            }
        }
        "remove" => {
            let v = form.value.trim();
            tags.retain(|t| !t.eq_ignore_ascii_case(v));
        }
        "add_suggested" => {
            for t in &manifest.suggested_tags {
                if !tags.iter().any(|e| e.eq_ignore_ascii_case(t)) {
                    tags.push(t.clone());
                }
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "bad op").into_response(),
    }
    tags.sort();
    tags.dedup();
    manifest.tags = tags.clone();
    if let Err(e) = crate::types::write_json(&manifest_path, &manifest) {
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")).into_response();
    }
    let _ = st.reindex().await;
    let suggested_tags: Vec<String> = manifest
        .suggested_tags
        .iter()
        .filter(|t| !tags.iter().any(|e| e.eq_ignore_ascii_case(t)))
        .cloned()
        .collect();
    let ctx = TagsCtx { rel: rel.to_string(), tags, suggested_tags };
    render(&TagsT { ctx })
}

/// POST /repos/{rel}/refresh: UI-triggered refresh with a polling element.
async fn refresh_ui(st: &Arc<AppState>, rel: &str) -> Response {
    if st.find_repo(rel).await.is_none() {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    }
    if !st.try_lock_repo(rel).await {
        return html("<div class=\"job-status failed\">a job is already running for this repo</div>".into());
    }
    let id = crate::server::spawn_refresh_job(st.clone(), rel.to_string(), "manual-refresh", None).await;
    let ctx = JobCtx {
        id,
        message: format!("Refreshing {rel}…"),
        done: false,
        failed: false,
        view_url: String::new(),
        oob_app: String::new(),
    };
    render(&JobT { ctx })
}

async fn repo_post(
    State(st): State<Arc<AppState>>,
    AxPath(rest): AxPath<String>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(rel) = rest.strip_suffix("/tags") {
        let edit = TagEditForm {
            op: form.get("op").cloned().unwrap_or_default(),
            value: form.get("value").cloned().unwrap_or_default(),
        };
        update_tags(&st, rel, edit).await
    } else if let Some(rel) = rest.strip_suffix("/refresh") {
        refresh_ui(&st, rel).await
    } else if let Some(rel) = rest.strip_suffix("/origin") {
        let edit = OriginForm { value: form.get("value").cloned().unwrap_or_default() };
        assign_origin_ui(&st, rel, edit).await
    } else if let Some(rel) = rest.strip_suffix("/metadata") {
        save_metadata_ui(&st, rel, form).await
    } else if let Some(rel) = rest.strip_suffix("/delete") {
        delete_repo_ui(&st, rel, form).await
    } else {
        (StatusCode::NOT_FOUND, "unknown action").into_response()
    }
}

/// POST /repos/{rel}/metadata (form): Jellyfin-style metadata editing:
/// name, description, notes, origin. Writes the manifest, reindexes, and
/// refreshes the page content in place.
async fn save_metadata_ui(st: &Arc<AppState>, rel: &str, form: HashMap<String, String>) -> Response {
    let Some(repo) = st.find_repo(rel).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    if !st.try_lock_repo(rel).await {
        return html(r#"<div class="import-error">a job is running for this repo</div>"#.into());
    }

    let mut new_rel = repo.rel.clone();
    let mut origin_changed = false;

    // origin: assign (and maybe relocate) if provided and different
    let new_origin = form.get("origin").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if new_origin.as_deref() != repo.manifest.origin.as_deref() && new_origin.is_some() {
        origin_changed = true;
        match crate::server::assign_origin(st, &repo, new_origin.as_deref().unwrap_or_default()).await {
            Ok(nr) => new_rel = nr,
            Err(e) => {
                st.unlock_repo(rel).await;
                return html(format!(r#"<div class="import-error">{}</div>"#, e.1));
            }
        }
    }

    // folder move (applies after origin relocation, so both compose)
    let desired_folder = form
        .get("folder")
        .map(|s| s.trim().trim_matches('/').to_string())
        .unwrap_or_default();
    if let Some(moved) = st.find_repo(&new_rel).await {
        match crate::server::move_repo_to_folder(st, &moved, &desired_folder).await {
            Ok(nr) => new_rel = nr,
            Err(e) => {
                st.unlock_repo(rel).await;
                return html(format!(r#"<div class="import-error">{e}</div>"#));
            }
        }
    }

    // plain metadata fields
    let manifest_path = if new_rel != repo.rel {
        st.root().await.join(&new_rel).join("repo.json")
    } else {
        repo.dir.join("repo.json")
    };
    if let Ok(mut manifest) = crate::types::read_json::<crate::types::RepoManifest>(&manifest_path) {
        if let Some(name) = form.get("name").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
            manifest.name = name;
        }
        manifest.description = form.get("description").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        manifest.notes = form.get("notes").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        // color radio always submits (empty = no color); a missing field leaves it alone
        if let Some(c) = form.get("color") {
            manifest.color = Some(c.trim().to_string()).filter(|s| !s.is_empty());
        }
        if let Err(e) = crate::types::write_json(&manifest_path, &manifest) {
            st.unlock_repo(rel).await;
            return html(format!(r#"<div class="import-error">cannot save manifest: {e:#}</div>"#));
        }
    }

    st.reindex().await.ok();
    st.unlock_repo(rel).await;

    // refresh the whole page content when the repo moved (URL changed)
    let moved = new_rel != repo.rel;
    let oob = oob_app_refresh(st).await;
    let msg = if origin_changed {
        format!(r#"&#10003; Metadata saved: origin set, repo is now <a href="/repos/{new_rel}">{new_rel}</a><br><span class="dim">Hit Force Refresh to clone it</span>"#)
    } else if moved {
        format!(r#"&#10003; Metadata saved: moved to <a href="/repos/{new_rel}">{new_rel}</a>"#)
    } else {
        "&#10003; Metadata saved".to_string()
    };
    html(format!(r#"<div class="job-status ok">{msg}</div>{oob}"#))
}

/// POST /repos/{rel}/origin (form): assign an origin to a repo (promotes
/// _unknown/ entries to their owner-repo folder and self-heals on refresh).
#[derive(Deserialize, Default)]
struct OriginForm {
    #[serde(default)] value: String, // origin URL
}

async fn assign_origin_ui(st: &Arc<AppState>, rel: &str, form: OriginForm) -> Response {
    let origin = form.value.trim().to_string();
    if origin.is_empty() {
        return html(r#"<div class="import-error">origin URL is required</div>"#.into());
    }
    let Some(repo) = st.find_repo(rel).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    if !st.try_lock_repo(rel).await {
        return html(r#"<div class="import-error">a job is running for this repo</div>"#.into());
    }
    let result = crate::server::assign_origin(st, &repo, &origin).await;
    st.unlock_repo(rel).await;
    match result {
        Ok(new_rel) => {
            let oob = oob_app_refresh(st).await;
            html(format!(
                r#"<div class="job-status ok">✓ Origin saved: repo is now <a href="/repos/{new_rel}">{new_rel}</a><br><span class="dim">Hit ⟳ Force Refresh to clone it</span></div>{oob}"#
            ))
        }
        Err(e) => html(format!(r#"<div class="import-error">{}</div>"#, e.1)),
    }
}

/// GET /repos/{rel}/delete: a centered modal that forces the user to type
/// the repository name before the (red) delete button unlocks.
async fn delete_confirm_ui(st: &Arc<AppState>, rel: &str) -> Response {
    let Some(repo) = st.find_repo(rel).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    let name = if repo.manifest.name.trim().is_empty() {
        repo.rel.clone()
    } else {
        repo.manifest.name.clone()
    };
    let rel_e = html_escape(&repo.rel);
    let name_e = html_escape(&name);
    html(format!(
        r##"<div class="modal-backdrop" id="delete-modal" onclick="if(event.target===this)this.remove()">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="delete-title">
    <h2 id="delete-title">Delete repository</h2>
    <p>Are you sure you want to delete <b>{name_e}</b>?</p>
    <p class="danger-text" style="margin:0.2rem 0 0.7rem">This is irreversible.</p>
    <form hx-post="/repos/{rel_e}/delete" hx-target="#toast" hx-swap="innerHTML" data-confirm="{name_e}" hx-indicator="find .spinner"
          hx-on::after-request="if(event.detail.xhr.getResponseHeader('X-Delete-OK')) document.getElementById('delete-modal').remove()">
      <label class="dim" for="delete-confirm">Type <b>{name_e}</b> to confirm</label>
      <input type="text" id="delete-confirm" name="confirm" autocomplete="off" autofocus
             oninput="this.form.querySelector('button[type=submit]').disabled = this.value.trim() !== this.form.dataset.confirm">
      <label style="display:block;margin:0.5rem 0"><input type="checkbox" name="files" value="1"> Permanently delete the archive files</label>
      <div class="modal-actions">
        <button type="button" class="dim-btn" onclick="document.getElementById('delete-modal').remove()">Cancel</button>
        <button type="submit" class="refresh-btn danger" disabled><span class="spinner htmx-indicator"></span>Delete</button>
      </div>
    </form>
  </div>
</div>"##
    ))
}

/// POST /repos/{rel}/delete: unregister (and optionally remove files), then
/// send the browser back to the index. Requires the typed name to match.
async fn delete_repo_ui(st: &Arc<AppState>, rel: &str, form: HashMap<String, String>) -> Response {
    let Some(repo) = st.find_repo(rel).await else {
        return (StatusCode::NOT_FOUND, "no such repo").into_response();
    };
    let expected = if repo.manifest.name.trim().is_empty() {
        repo.rel.clone()
    } else {
        repo.manifest.name.clone()
    };
    let typed = form.get("confirm").map(|s| s.trim().to_string()).unwrap_or_default();
    if typed != expected.trim() {
        // 200 so the toast swaps in; no X-Delete-OK, so the modal stays open.
        return html(r#"<div class="job-status failed">Type the repository name exactly to confirm deletion.</div>"#.into());
    }
    let delete_files = form
        .get("files")
        .map(|v| v == "1" || v == "on" || v == "true")
        .unwrap_or(false);
    match crate::server::delete_repo(st, &repo, delete_files).await {
        Ok(()) => {
            let msg = if delete_files {
                "Deleted repository and its files"
            } else {
                "Removed repository from the index (files kept)"
            };
            let mut resp = html(format!(
                r#"<div class="job-status ok">{msg}</div>{}"#,
                oob_app_refresh(st).await
            ));
            resp.headers_mut()
                .insert("HX-Redirect", axum::http::HeaderValue::from_static("/"));
            resp.headers_mut()
                .insert("X-Delete-OK", axum::http::HeaderValue::from_static("1"));
            resp
        }
        Err(e) => html(format!(r#"<div class="import-error">{}</div>"#, e.1)),
    }
}

// page_url re-exported for template use? (templates reference prebuilt URLs)
#[allow(unused)]
fn _keep_page_url(tags: &[String], q: &str, folder: &str) -> String {
    page_url(tags, q, folder)
}
// ---------- folders (sidebar tree management) ----------

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// GET /folders/options: <option> list for the topbar's folder datalist
/// (the topbar has no template context, so it fetches its folder list itself).
async fn folders_options(State(st): State<Arc<AppState>>) -> Response {
    let paths = { let index = st.index.read().await; views::all_folder_paths(&index) };
    let opts: String = paths
        .iter()
        .map(|p| format!("<option value=\"{}\"></option>", html_escape(p)))
        .collect();
    html(opts)
}

#[derive(Deserialize, Default)]
struct MoveForm {
    #[serde(default)] rel: String,
    #[serde(default)] folder: String,
}

/// POST /repos/move: drag-and-drop target. Moves a repo into a folder and
/// answers with an out-of-bounds #app refresh; errors land in #toast.
async fn repo_move(State(st): State<Arc<AppState>>, Form(form): Form<MoveForm>) -> Response {
    let err = |msg: String| html(format!(r#"<div class="import-error">{}</div>"#, html_escape(&msg)));
    let rel = form.rel.trim().trim_matches('/').to_string();
    let Some(repo) = st.find_repo(&rel).await else {
        return err(format!("no such repo: {rel}"));
    };
    if !st.try_lock_repo(&rel).await {
        return err(format!("a job is running for {rel}; try again when it finishes"));
    }
    let result = crate::server::move_repo_to_folder(&st, &repo, &form.folder).await;
    st.unlock_repo(&rel).await;
    match result {
        Ok(_) => {
            st.reindex().await.ok();
            html(format!("{}{}", oob_toast("Repository moved"), oob_app_refresh(&st).await))
        }
        Err(e) => err(e),
    }
}

// ---------- bulk operations ----------

#[derive(Deserialize, Default)]
struct BulkForm {
    #[serde(default)]
    action: String,
    /// JSON array of repo rel paths, built by the selection JS.
    #[serde(default)]
    repos: String,
    #[serde(default)]
    tag: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    files: String,
}

fn bulk_toast(ok: bool, msg: &str) -> Response {
    let cls = if ok { "ok" } else { "failed" };
    html(format!(
        r#"<div class="job-status {cls}">{}</div>"#,
        html_escape(msg)
    ))
}

/// A success toast plus an out-of-bounds refresh of the grid (which also
/// resets the selection UI) and a close for the bulk modal, if one is open.
async fn bulk_done(st: &Arc<AppState>, msg: String) -> Response {
    html(format!(
        r#"<div class="job-status ok">{}</div>{}{}"#,
        html_escape(&msg),
        oob_close_modal("bulk-modal"),
        oob_app_refresh(st).await
    ))
}

fn skip_note(skipped: usize) -> String {
    if skipped == 0 {
        String::new()
    } else {
        format!(" ({skipped} skipped: missing or busy)")
    }
}

/// POST /repos/bulk: apply one action to the repositories selected in the UI.
async fn bulk_action(State(st): State<Arc<AppState>>, Form(form): Form<BulkForm>) -> Response {
    let rels: Vec<String> = serde_json::from_str::<Vec<String>>(&form.repos)
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.trim().trim_matches('/').to_string())
        .filter(|r| !r.is_empty())
        .collect();
    if rels.is_empty() {
        return bulk_toast(false, "Select at least one repository");
    }
    match form.action.as_str() {
        "refresh" => bulk_refresh(&st, rels).await,
        "tag_add" => bulk_tag(&st, &rels, &form.tag, true).await,
        "tag_remove" => bulk_tag(&st, &rels, &form.tag, false).await,
        "move" => bulk_move(&st, &rels, &form.folder).await,
        "delete" => bulk_delete(&st, &rels, form.files == "1").await,
        _ => bulk_toast(false, "Unknown bulk action"),
    }
}

/// GET /repos/bulk/tag: a modal to add or remove one tag on the selection.
async fn bulk_tag_modal() -> Response {
    html(
        r##"<div class="modal-backdrop" id="bulk-modal" onclick="if(event.target===this)this.remove()">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bulk-tag-title">
    <h2 id="bulk-tag-title">Edit tags</h2>
    <p class="dim">Applied to the selected repositories.</p>
    <label for="bulk-tag-input">Tag</label>
    <input id="bulk-tag-input" type="text" autocomplete="off" placeholder="e.g. cli" autofocus>
    <div class="modal-actions">
      <button type="button" class="dim-btn" onclick="document.getElementById('bulk-modal').remove()">Cancel</button>
      <button type="button" class="dim-btn" hx-post="/repos/bulk" hx-target="#toast" hx-swap="innerHTML"
              hx-include="#bulk-tag-input"
              hx-vals='js:{action:"tag_remove", repos: JSON.stringify(selectedRepos())}'>Remove</button>
      <button type="button" class="refresh-btn" hx-post="/repos/bulk" hx-target="#toast" hx-swap="innerHTML"
              hx-include="#bulk-tag-input"
              hx-vals='js:{action:"tag_add", repos: JSON.stringify(selectedRepos())}'><span class="spinner htmx-indicator"></span>Add</button>
    </div>
  </div>
</div>"##
            .to_string(),
    )
}

/// GET /repos/bulk/move: a modal to move the selection into a folder.
async fn bulk_move_modal() -> Response {
    html(
        r##"<div class="modal-backdrop" id="bulk-modal" onclick="if(event.target===this)this.remove()">
  <div class="modal" role="dialog" aria-modal="true" aria-labelledby="bulk-move-title">
    <h2 id="bulk-move-title">Move to folder</h2>
    <p class="dim">Moves the selected repositories. Leave empty for the archive root.</p>
    <label for="bulk-move-folder">Folder</label>
    <input id="bulk-move-folder" type="text" list="bulk-move-list" autocomplete="off" placeholder="e.g. tools/rust" autofocus>
    <datalist id="bulk-move-list" hx-get="/folders/options" hx-trigger="load" hx-target="this" hx-swap="innerHTML"></datalist>
    <div class="modal-actions">
      <button type="button" class="dim-btn" onclick="document.getElementById('bulk-modal').remove()">Cancel</button>
      <button type="button" class="refresh-btn" hx-post="/repos/bulk" hx-target="#toast" hx-swap="innerHTML"
              hx-include="#bulk-move-folder"
              hx-vals='js:{action:"move", repos: JSON.stringify(selectedRepos())}'><span class="spinner htmx-indicator"></span>Move</button>
    </div>
  </div>
</div>"##
            .to_string(),
    )
}

/// Queue a refresh for each selected repo. A coordinator task takes permits
/// from the shared semaphore, so a bulk refresh honours `max_concurrent`
/// instead of starting every clone at once.
async fn bulk_refresh(st: &Arc<AppState>, rels: Vec<String>) -> Response {
    let total = rels.len();
    let st2 = st.clone();
    tokio::spawn(async move {
        for rel in rels {
            if !st2.try_lock_repo(&rel).await {
                continue;
            }
            let permit = st2.refresh_sem.clone().acquire_owned().await.ok();
            crate::server::spawn_refresh_job(st2.clone(), rel, "manual-refresh", permit).await;
        }
    });
    tracing::info!(count = total, "bulk refresh queued");
    bulk_done(st, format!("Refresh queued for {total} repositories")).await
}

/// Add or remove one tag in a repo manifest. `Ok(true)` when the file changed.
async fn edit_tags(st: &Arc<AppState>, rel: &str, tag: &str, add: bool) -> Result<bool, ()> {
    let Some(repo) = st.find_repo(rel).await else {
        return Err(());
    };
    let path = repo.dir.join("repo.json");
    let Ok(mut manifest) = crate::types::read_json::<crate::types::RepoManifest>(&path) else {
        return Err(());
    };
    let before = manifest.tags.clone();
    if add {
        if !manifest.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            manifest.tags.push(tag.to_string());
        }
    } else {
        manifest.tags.retain(|t| !t.eq_ignore_ascii_case(tag));
    }
    manifest.tags.sort();
    manifest.tags.dedup();
    if manifest.tags == before {
        return Ok(false);
    }
    crate::types::write_json(&path, &manifest).map_err(|_| ())?;
    Ok(true)
}

async fn bulk_tag(st: &Arc<AppState>, rels: &[String], tag: &str, add: bool) -> Response {
    let tag = tag.trim();
    if tag.is_empty() {
        return bulk_toast(false, "Enter a tag first");
    }
    let mut changed = 0usize;
    let mut skipped = 0usize;
    for rel in rels {
        if !st.try_lock_repo(rel).await {
            skipped += 1;
            continue;
        }
        let result = edit_tags(st, rel, tag, add).await;
        st.unlock_repo(rel).await;
        match result {
            Ok(true) => changed += 1,
            Ok(false) => {}
            Err(()) => skipped += 1,
        }
    }
    st.reindex().await.ok();
    let verb = if add { "added to" } else { "removed from" };
    tracing::info!(tag, add, changed, skipped, "bulk tag applied");
    bulk_done(st, format!("Tag '{tag}' {verb} {changed} repositories{}", skip_note(skipped))).await
}

async fn bulk_move(st: &Arc<AppState>, rels: &[String], folder: &str) -> Response {
    let folder = folder.trim().trim_matches('/').to_string();
    let mut moved = 0usize;
    let mut skipped = 0usize;
    for rel in rels {
        let Some(repo) = st.find_repo(rel).await else {
            skipped += 1;
            continue;
        };
        if !st.try_lock_repo(rel).await {
            skipped += 1;
            continue;
        }
        let result = crate::server::move_repo_to_folder(st, &repo, &folder).await;
        st.unlock_repo(rel).await;
        match result {
            Ok(_) => moved += 1,
            Err(_) => skipped += 1,
        }
    }
    st.reindex().await.ok();
    let dest = if folder.is_empty() { "the root".to_string() } else { format!("/{folder}") };
    tracing::info!(folder = %folder, moved, skipped, "bulk move done");
    bulk_done(st, format!("Moved {moved} repositories to {dest}{}", skip_note(skipped))).await
}

async fn bulk_delete(st: &Arc<AppState>, rels: &[String], delete_files: bool) -> Response {
    let mut deleted = 0usize;
    let mut skipped = 0usize;
    for rel in rels {
        let Some(repo) = st.find_repo(rel).await else {
            skipped += 1;
            continue;
        };
        // delete_repo takes the repo lock itself, so do not hold it here.
        match crate::server::delete_repo(st, &repo, delete_files).await {
            Ok(()) => deleted += 1,
            Err(_) => skipped += 1,
        }
    }
    st.reindex().await.ok();
    let kept = if delete_files { "with their files" } else { "(files kept)" };
    tracing::info!(deleted, skipped, delete_files, "bulk delete done");
    bulk_done(st, format!("Removed {deleted} repositories {kept}{}", skip_note(skipped))).await
}

/// GET /folders/new: the create form (parent dropdown from the index).
/// `?parent=…` preselects the folder that is currently open in the sidebar.
async fn folders_new(
    State(st): State<Arc<AppState>>,
    Query(map): Query<HashMap<String, String>>,
) -> Response {
    let requested = map.get("parent").cloned().unwrap_or_default();
    let ctx = {
        let index = st.index.read().await;
        let requested = requested.trim().trim_matches('/').to_string();
        let parent = if !requested.is_empty() && views::valid_folder_path(&requested, &index) {
            requested
        } else {
            String::new()
        };
        let parents = views::all_folder_paths(&index)
            .into_iter()
            .map(|path| views::FolderParentOption { selected: path == parent, path })
            .collect();
        FolderFormCtx {
            mode: "new".into(),
            rel: String::new(),
            update_url: "/folders/create".into(),
            name: String::new(),
            parent,
            parents,
            icon: String::new(),
            count: 0,
        }
    };
    render(&FolderFormT { ctx })
}

/// GET /folders/close: empty response; the cancel button clears the slot.
async fn folders_close() -> Response {
    html(String::new())
}

/// GET /folders/edit?rel=…: the edit form for an existing folder.
async fn folders_edit_form(
    State(st): State<Arc<AppState>>,
    Query(map): Query<HashMap<String, String>>,
) -> Response {
    let rel = map.get("rel").cloned().unwrap_or_default();
    let ctx = {
        let index = st.index.read().await;
        let is_known = views::valid_folder_path(&rel, &index);
        let icon = index
            .folders
            .iter()
            .find(|f| f.rel == rel)
            .and_then(|f| f.manifest.icon.clone())
            .unwrap_or_default();
        let count = index
            .repos
            .iter()
            .filter(|r| r.rel.starts_with(&format!("{rel}/")))
            .count();
        (is_known, icon, count)
    };
    if !ctx.0 {
        return html("<div class=\"import-error\">no such folder</div>".into());
    }
    let name = rel.rsplit('/').next().unwrap_or(&rel).to_string();
    render(&FolderFormT {
        ctx: FolderFormCtx {
            mode: "edit".into(),
            rel: rel.clone(),
            update_url: "/folders/update".into(),
            name,
            parent: String::new(),
            parents: Vec::new(),
            icon: ctx.1,
            count: ctx.2,
        },
    })
}

#[derive(Deserialize, Default)]
struct FolderCreateForm {
    #[serde(default)] parent: String,
    #[serde(default)] name: String,
    #[serde(default)] icon: String,
}

/// POST /folders/create: mkdir + folder.json, then refresh the sidebar via OOB.
async fn folders_create(
    State(st): State<Arc<AppState>>,
    Form(form): Form<FolderCreateForm>,
) -> Response {
    let err = |msg: String| html(format!(r#"<div class="import-error">{msg}</div>"#));
    let name = form.name.trim().to_string();
    let parent = form.parent.trim().trim_matches('/').to_string();
    if !crate::server::valid_folder_name(&name) {
        return err("invalid folder name (no slashes, not hidden, not _unknown)".into());
    }
    if !parent.is_empty() {
        let ok = { let index = st.index.read().await; views::valid_folder_path(&parent, &index) };
        if !ok {
            return err(format!("unknown parent folder: {parent}"));
        }
    }
    let rel = if parent.is_empty() { name.clone() } else { format!("{parent}/{name}") };
    let root = st.root().await;
    let dir = root.join(&rel);
    if dir.exists() {
        return err(format!("{rel} already exists"));
    }
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return err(format!("cannot create folder: {e}"));
    }
    let icon = icon_first_char(form.icon.trim());
    let manifest = crate::types::FolderManifest { icon: Some(icon).filter(|s| !s.is_empty()) };
    if let Err(e) = crate::types::write_json(&dir.join("folder.json"), &manifest) {
        return err(format!("cannot write folder.json: {e}"));
    }
    st.reindex().await.ok();
    html(format!(
        "{}{}{}",
        oob_close_modal("folder-modal"),
        oob_toast("Folder created"),
        oob_app_refresh(&st).await
    ))
}

#[derive(Deserialize, Default)]
struct FolderUpdateForm {
    #[serde(default)] rel: String,
    #[serde(default)] op: String,
    #[serde(default)] name: String,
    #[serde(default)] icon: String,
}

/// POST /folders/update: rename, set icon, or delete (only when empty).
async fn folders_update(
    State(st): State<Arc<AppState>>,
    Form(form): Form<FolderUpdateForm>,
) -> Response {
    let err = |msg: String| html(format!(r#"<div class="import-error">{msg}</div>"#));
    let rel = form.rel.trim().trim_matches('/').to_string();
    let root = st.root().await;
    let dir = root.join(&rel);

    let (is_known, count) = {
        let index = st.index.read().await;
        let known = views::valid_folder_path(&rel, &index);
        let count = index.repos.iter().filter(|r| r.rel.starts_with(&format!("{rel}/"))).count();
        (known, count)
    };
    if !is_known {
        return err(format!("no such folder: {rel}"));
    }

    if form.op == "delete" {
        if count > 0 {
            return err(format!("folder holds {count} repos; move them out first"));
        }
        if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
            return err(format!("cannot delete folder: {e}"));
        }
        st.reindex().await.ok();
        return html(format!(
            "{}{}{}",
            oob_close_modal("folder-modal"),
            oob_toast("Folder deleted"),
            oob_app_refresh(&st).await
        ));
    }

    // save: rename when the name changed
    let name = form.name.trim().to_string();
    if !crate::server::valid_folder_name(&name) {
        return err("invalid folder name (no slashes, not hidden, not _unknown)".into());
    }
    let parent = rel.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let new_rel = if parent.is_empty() { name.clone() } else { format!("{parent}/{name}") };
    if new_rel != rel {
        let new_dir = root.join(&new_rel);
        if new_dir.exists() {
            return err(format!("{new_rel} already exists"));
        }
        if let Err(e) = tokio::fs::rename(&dir, &new_dir).await {
            return err(format!("cannot rename folder: {e}"));
        }
    }
    let dir = root.join(&new_rel);
    let icon = icon_first_char(form.icon.trim());
    let manifest = crate::types::FolderManifest { icon: Some(icon).filter(|s| !s.is_empty()) };
    if let Err(e) = crate::types::write_json(&dir.join("folder.json"), &manifest) {
        return err(format!("cannot write folder.json: {e}"));
    }
    st.reindex().await.ok();
    html(format!(
        "{}{}{}",
        oob_close_modal("folder-modal"),
        oob_toast("Folder saved"),
        oob_app_refresh(&st).await
    ))
}

// ---------- settings ----------

async fn settings_page(State(st): State<Arc<AppState>>) -> Response {
    let cfg = st.cfg().await;
    render(&views::SettingsT { ctx: views::settings_ctx_from(&cfg, false) })
}

/// GET /stats: totals + a 7-day chart + recent days, no collector required.
async fn stats_page(State(st): State<Arc<AppState>>) -> Response {
    let (m, t) = st.stats_snapshot().await;
    let cfg = st.cfg().await;
    render(&views::StatsT { ctx: views::stats_ctx_from(&m, &t, &cfg) })
}

/// POST /settings/verify: kick off a full archive verify as a background job.
async fn settings_verify(State(st): State<Arc<AppState>>) -> Response {
    let id = crate::server::spawn_verify_job(&st).await;
    render(&views::VerifyT { ctx: views::verify_ctx_running(id) })
}

/// GET /settings/verify/{id}: polling fragment (progress, then the report).
async fn settings_verify_status(
    State(st): State<Arc<AppState>>,
    AxPath(id): AxPath<u64>,
) -> Response {
    render(&views::VerifyT { ctx: views::verify_ctx_from(&st, id).await })
}

#[derive(Default)]
struct SettingsForm(HashMap<String, String>);

fn num<T: std::str::FromStr>(f: &SettingsForm, key: &str, default: T) -> T {
    f.0.get(key).and_then(|v| v.trim().parse().ok()).unwrap_or(default)
}

/// POST /settings: update the live config and persist it to the config file.
async fn settings_save(
    State(st): State<Arc<AppState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let f = SettingsForm(form);
    let mut cfg = st.cfg().await;

    cfg.retention.keep_branch_snapshots = num(&f, "keep_branch", cfg.retention.keep_branch_snapshots);
    cfg.retention.keep_releases = num(&f, "keep_releases", cfg.retention.keep_releases);
    cfg.scheduler.enabled = f.0.get("sched_enabled").map(|v| v == "1").unwrap_or(false);
    cfg.scheduler.default_interval_days = num(&f, "interval_days", cfg.scheduler.default_interval_days).max(1);
    cfg.scheduler.release_poll_hours = num(&f, "poll_hours", cfg.scheduler.release_poll_hours).max(1);
    cfg.scheduler.poll_every_secs = num(&f, "poll_every", cfg.scheduler.poll_every_secs).max(60);
    cfg.scheduler.dead_after_days = num(&f, "dead_after", cfg.scheduler.dead_after_days);
    cfg.verify.enabled = f.0.get("verify_enabled").map(|v| v == "1").unwrap_or(false);
    cfg.verify.interval_days = num(&f, "verify_interval_days", cfg.verify.interval_days).max(1);
    cfg.llm.enabled = f.0.get("llm_enabled").map(|v| v == "1").unwrap_or(false);
    if let Some(url) = f.0.get("llm_url") {
        if !url.trim().is_empty() {
            cfg.llm.url = url.trim().to_string();
        }
    }
    cfg.llm.model = f.0.get("llm_model").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    cfg.llm.batch_size = num(&f, "llm_batch", cfg.llm.batch_size).max(1);
    cfg.llm.disable_thinking = f.0.get("disable_thinking").map(|v| v == "1").unwrap_or(false);
    cfg.otel.enabled = f.0.get("otel_enabled").map(|v| v == "1").unwrap_or(false);
    if let Some(ep) = f.0.get("otel_endpoint") {
        if !ep.trim().is_empty() {
            cfg.otel.endpoint = ep.trim().to_string();
        }
    }
    if let Some(sn) = f.0.get("otel_service") {
        if !sn.trim().is_empty() {
            cfg.otel.service_name = sn.trim().to_string();
        }
    }
    cfg.otel.interval_secs = num(&f, "otel_interval", cfg.otel.interval_secs).max(10);

    // release binaries: checkboxes for the common platforms + a free-text field.
    // Canonicalize so "win64" is stored as "windows-x64" and duplicates merge.
    let mut platforms: Vec<String> = Vec::new();
    if f.0.get("release_all").map(|v| v == "1").unwrap_or(false) {
        platforms.push("all".to_string());
    }
    for choice in crate::platform::CHOICES {
        if f.0.get(&format!("plat_{}", choice.slug)).map(|v| v == "1").unwrap_or(false) {
            platforms.push(choice.slug.to_string());
        }
    }
    if let Some(extra) = f.0.get("release_platforms_extra") {
        for token in extra.split([',', ' ', ';', '\n', '\t']).filter(|t| !t.trim().is_empty()) {
            if let Some(c) = crate::platform::canonical(token) {
                platforms.push(c);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    platforms.retain(|p| seen.insert(p.clone()));
    cfg.releases.platforms = platforms;
    cfg.releases.max_asset_mb = num(&f, "release_max_asset_mb", cfg.releases.max_asset_mb);
    cfg.tags.take_suggested = f.0.get("take_suggested_tags").map(|v| v == "1").unwrap_or(false);
    cfg.github.fetch_avatars = f.0.get("fetch_avatars").map(|v| v == "1").unwrap_or(false);

    // apply live
    *st.cfg.write().await = cfg.clone();
    // persist (if we know where the config lives)
    match &st.config_path {
        Some(path) => {
            if let Err(e) = cfg.save(path) {
                return html(format!(r#"<div class="import-error">cannot save config: {e:#}</div>"#));
            }
            tracing::info!(config = %path.display(), "settings updated");
        }
        None => tracing::warn!("settings changed in memory only (config path unknown)"),
    }
    // re-render the panel (saved flag off: feedback goes to the toast) while
    // an OOB toast confirms the save at the bottom of the viewport
    let page = views::SettingsT { ctx: views::settings_ctx_from(&cfg, false) }
        .render()
        .unwrap_or_default();
    html(format!("{}{page}", oob_toast("Settings saved")))
}

// ---------- import ----------

async fn import_page() -> Response {
    render(&views::ImportT)
}

/// POST /import/scan (form: dir) → scan offline, store, return review form.
async fn import_scan(
    State(st): State<Arc<AppState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let dir = form.get("dir").map(|s| s.trim().to_string()).unwrap_or_default();
    if dir.is_empty() {
        return html("<div class=\"import-error\">Directory path is required.</div>".into());
    }
    match crate::importer::scan_dir(std::path::Path::new(&dir)) {
        Ok(scan) => {
            let id = st.store_scan(scan.clone()).await;
            let ctx = views::build_import_ctx(&st, id, &scan).await;
            render(&views::ImportRowsT { ctx })
        }
        Err(e) => html(format!("<div class=\"import-error\">Scan failed: {e:#}</div>")),
    }
}

/// GitHub repository search → candidate origins (web search assist). Goes
/// through the shared governor and conditional cache like every other forge
/// call, so repeated searches revalidate instead of refetching.
async fn github_search(st: &Arc<AppState>, query: &str) -> anyhow::Result<Vec<views::CandView>> {
    let cfg = st.cfg().await;
    let url = format!(
        "https://api.github.com/search/repositories?q={}&per_page=5",
        views::urlencode(query)
    );
    let body = crate::httpcache::conditional_get(&cfg, &st.governor, &st.http_cache, &url, || {
        let mut req = crate::ratelimit::api_client()
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "reposilo-import");
        if let Some(tok) = cfg.github.resolved_token() {
            req = req.bearer_auth(tok);
        }
        req
    })
    .await
    .ok_or_else(|| anyhow::anyhow!("GitHub API request failed (rate limit? add [github] token in config)"))?
    .body;
    let body: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    let items = body["items"].as_array().cloned().unwrap_or_default();
    Ok(items
        .into_iter()
        .map(|i| views::CandView {
            full_name: i["full_name"].as_str().unwrap_or_default().to_string(),
            url: i["clone_url"].as_str().unwrap_or_default().to_string(),
            description: i["description"].as_str().unwrap_or_default().to_string(),
            stars: i["stargazers_count"].as_u64().unwrap_or(0),
        })
        .collect())
}

#[derive(Deserialize)]
struct ImportSuggestQuery {
    scan: u64,
    i: usize,
    #[serde(default)] q: String,
}

/// GET /import/suggest?scan=&i=&q= → candidate list for one row.
async fn import_suggest(
    State(st): State<Arc<AppState>>,
    Query(q): Query<ImportSuggestQuery>,
) -> Response {
    let Some(scan) = st.scans.lock().await.get(&q.scan).cloned() else {
        return html("<div class=\"import-error\">Unknown scan (rescan the directory).</div>".into());
    };
    let Some(row) = scan.rows.get(q.i) else {
        return html("<div class=\"import-error\">Unknown row.</div>".into());
    };
    let query = if q.q.is_empty() {
        row.detected_name
            .clone()
            .unwrap_or_else(|| row.file_name.trim_end_matches(".zip").to_string())
    } else {
        q.q
    };
    let scan_id = q.scan;
    let i = q.i;
    match github_search(&st, &query).await {
        Ok(candidates) => render(&views::SuggestT {
            ctx: views::SuggestCtx {
                i,
                scan_id,
                query,
                candidates,
                llm_name: String::new(),
                llm_queries: Vec::new(),
                error: String::new(),
            },
        }),
        Err(e) => render(&views::SuggestT {
            ctx: views::SuggestCtx {
                i,
                scan_id,
                query,
                candidates: Vec::new(),
                llm_name: String::new(),
                llm_queries: Vec::new(),
                error: format!("{e:#}"),
            },
        }),
    }
}

#[derive(Deserialize, Default)]
struct ImportLlmForm {
    #[serde(default)] scan: String,
    #[serde(default)] i: String,
}

/// POST /import/llm (form: scan, i) → llama.cpp identifies the zip from its
/// fingerprints (README excerpt + file listing + detected name) and proposes
/// a name + GitHub search queries.
async fn import_llm(
    State(st): State<Arc<AppState>>,
    Form(form): Form<ImportLlmForm>,
) -> Response {
    if !st.cfg().await.llm.enabled {
        return html("<div class=\"import-error\">llama.cpp not enabled in config ([llm] enabled = true + url).</div>".into());
    }
    let (Ok(scan_id), Ok(i)) = (form.scan.parse::<u64>(), form.i.parse::<usize>()) else {
        return html("<div class=\"import-error\">Bad scan/row.</div>".into());
    };
    let Some(scan) = st.scans.lock().await.get(&scan_id).cloned() else {
        return html("<div class=\"import-error\">Unknown scan (rescan the directory).</div>".into());
    };
    let Some(row) = scan.rows.get(i) else {
        return html("<div class=\"import-error\">Unknown row.</div>".into());
    };

    let prompt = format!(
        "You are identifying a zipped git repository from a personal archive collection.\n\
         Facts:\n- zip file name: {}\n- detected name: {}\n- top-level files: {}\n- README excerpt:\n{}\n\n\
         Respond ONLY with a JSON object of the form \
         {{\"name\":\"<best guess of the project name>\",\"queries\":[\"<github search query to find this repo>\",\"<second query>\"]}}.",
        row.file_name,
        row.detected_name.clone().unwrap_or_default(),
        row.file_sample.join(", "),
        row.readme_excerpt.chars().take(600).collect::<String>()
    );
    let tagger = crate::llm::LlmTagger::new(&st.cfg().await.llm);
    match tagger.chat(&prompt).await {
        Ok(content) => {
            let ident = crate::llm::parse_llm_identify(&content);
            let llm_name = ident.name.clone().unwrap_or_default();
            let llm_queries: Vec<views::QueryLink> = ident
                .queries
                .iter()
                .map(|q| views::QueryLink {
                    q: q.clone(),
                    url: format!("/import/suggest?scan={scan_id}&i={i}&q={}", views::urlencode(q)),
                })
                .collect();
            render(&views::SuggestT {
                ctx: views::SuggestCtx {
                    i,
                    scan_id,
                    query: llm_name.clone(),
                    candidates: Vec::new(),
                    llm_name,
                    llm_queries,
                    error: String::new(),
                },
            })
        }
        Err(e) => render(&views::SuggestT {
            ctx: views::SuggestCtx {
                i,
                scan_id,
                query: String::new(),
                candidates: Vec::new(),
                llm_name: String::new(),
                llm_queries: Vec::new(),
                error: format!("llama.cpp failed: {e:#}"),
            },
        }),
    }
}



/// POST /import/commit (rows via include_i / origin_i / tags_i / unknown_i) →
/// background job that moves/copies zips and writes manifests + sidecars.
async fn import_commit(
    State(st): State<Arc<AppState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let Ok(scan_id) = form.get("scan").cloned().unwrap_or_default().parse::<u64>() else {
        return html("<div class=\"import-error\">Bad scan id.</div>".into());
    };
    let keep_original = form.get("keep").map(|v| v == "1").unwrap_or(false);
    let Some(scan) = st.scans.lock().await.get(&scan_id).cloned() else {
        return html("<div class=\"import-error\">Unknown scan (rescan the directory).</div>".into());
    };

    // collect user decisions from indexed form fields
    let mut rows: Vec<crate::importer::ImportRow> = Vec::new();
    for i in 0..scan.rows.len() {
        if form.get(&format!("include_{i}")).map(|v| v == "1").unwrap_or(false) {
            rows.push(crate::importer::ImportRow {
                i,
                origin: form
                    .get(&format!("origin_{i}"))
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty()),
                unknown: form.get(&format!("unknown_{i}")).map(|v| v == "1").unwrap_or(false),
                tags: form
                    .get(&format!("tags_{i}"))
                    .map(|s| {
                        s.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    if rows.is_empty() {
        return html("<div class=\"import-error\">No rows selected.</div>".into());
    }

    let id = st.create_job("import", None).await;
    let st2 = st.clone();
    let cfg = st.cfg().await;
    tokio::spawn(async move {
        let mut outcomes: Vec<crate::importer::ImportOutcome> = Vec::new();
        for row in rows {
            let Some(scan_row) = scan.rows.get(row.i) else { continue };
            outcomes
                .push(crate::importer::import_one(&cfg, scan_row, row.origin.as_deref(), &row.tags, row.unknown, keep_original).await);
        }
        let imported = outcomes.iter().filter(|o| o.action == "imported").count();
        let parked = outcomes.iter().filter(|o| o.action == "unknown-parked").count();
        let failed = outcomes.iter().filter(|o| o.action == "failed").count();
        let note = format!("Imported {imported}, parked {parked} unknown, {failed} failed");
        let summary = if failed > 0 {
            let details: Vec<String> = outcomes
                .iter()
                .filter(|o| o.action == "failed")
                .map(|o| format!("{}: {}", o.file, o.detail))
                .collect();
            format!("{note}: {}", details.join("; "))
        } else {
            note.clone()
        };
        st2.set_job_note(id, summary).await;
        tracing::info!("{note}");
        let _ = st2.reindex().await;
        st2.finish_job(id, None).await;
    });

    let ctx = views::JobCtx {
        id,
        message: "Importing…".into(),
        done: false,
        failed: false,
        view_url: String::new(),
        oob_app: String::new(),
    };
    render(&views::JobT { ctx })
}

// ---------- notifications page (bell target) ----------


async fn notifications_page(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let items: Vec<crate::server::Notification> =
        st.notifications.lock().await.iter().take(100).cloned().collect();
    let user = crate::auth::current_user(&st, &headers);
    let can_mark_read = user.is_some();
    let views: Vec<views::NotifView> = items
        .iter()
        .map(|n| {
            let (at, at_exact) = views::humanize_ago(&n.at).unwrap_or_else(|| {
                let t: String = n.at.chars().take(16).collect();
                (t.clone(), t)
            });
            views::NotifView {
                kind: n.kind.clone(),
                repo: n.repo.clone(),
                title: n.title.clone(),
                at,
                at_exact,
                changelog_html: n
                    .body
                    .as_deref()
                    .map(crate::files::markdown_to_html)
                    .unwrap_or_default(),
            }
        })
        .collect();
    let ctx = views::NotificationsCtx { items: views, can_mark_read };
    render(&views::NotificationsT { ctx })
}

async fn notifications_mark_read(State(st): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(user) = crate::auth::current_user(&st, &headers) {
        let _ = crate::auth::set_last_seen(&st.root().await, &user);
    }
    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header("location", "/notifications")
        .body(Body::empty())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::{icon_first_char, sniff_image};

    #[test]
    fn sniffs_common_image_types() {
        assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n"), "image/png");
        assert_eq!(sniff_image(b"\xFF\xD8\xFF\xE0"), "image/jpeg");
        assert_eq!(sniff_image(b"GIF89a"), "image/gif");
        assert_eq!(sniff_image(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "image/webp");
        assert_eq!(sniff_image(b"not an image"), "application/octet-stream");
    }

    #[test]
    fn icon_keeps_only_the_first_character() {
        assert_eq!(icon_first_char(""), "");
        assert_eq!(icon_first_char("a"), "a");
        assert_eq!(icon_first_char("hello"), "h");
        assert_eq!(icon_first_char("🎮"), "🎮");
        // variation selector stays with its base
        assert_eq!(icon_first_char("🕹️"), "🕹️");
        // ZWJ sequence is preserved as one visible character
        assert_eq!(icon_first_char("👨‍👩‍👧‍👦"), "👨‍👩‍👧‍👦");
        // leading whitespace is dropped by the caller's trim; here it is kept
        assert_eq!(icon_first_char("1abc"), "1");
    }
}
