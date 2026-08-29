//! View-model structs and builders for the HTMX UI (askama templates).

use std::sync::Arc;

use askama::Template;

use crate::server::AppState;

// ---------- helpers ----------

pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(format!("%{b:02X}").as_str()),
        }
    }
    out
}

/// GitHub-style language color class for the card dot.
fn lang_class(name: &str) -> &'static str {
    match name {
        "Rust" => "lang-rust",
        "C" => "lang-c",
        "C++" => "lang-cpp",
        "C#" => "lang-csharp",
        "Python" => "lang-python",
        "JavaScript" => "lang-javascript",
        "TypeScript" => "lang-typescript",
        "Go" => "lang-go",
        "Java" => "lang-java",
        "Ruby" => "lang-ruby",
        "PHP" => "lang-php",
        "Shell" => "lang-shell",
        "Swift" => "lang-swift",
        "Kotlin" => "lang-kotlin",
        "Zig" => "lang-zig",
        "Lua" => "lang-lua",
        "Assembly" => "lang-assembly",
        "Nim" => "lang-nim",
        "Dart" => "lang-dart",
        "Elixir" => "lang-elixir",
        "Haskell" => "lang-haskell",
        "Scala" => "lang-scala",
        _ => "",
    }
}

pub fn human_bytes(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {u}", u = units[u])
    }
}

/// Full page URL with the given filter state, e.g. "/?tags=a,q=foo".
pub fn page_url(tags: &[String], q: &str, folder: &str) -> String {
    let mut params = Vec::new();
    if !tags.is_empty() {
        params.push(format!("tags={}", urlencode(&tags.join(","))));
    }
    if !q.is_empty() {
        params.push(format!("q={}", urlencode(q)));
    }
    if !folder.is_empty() {
        params.push(format!("folder={}", urlencode(folder)));
    }
    if params.is_empty() {
        "/".to_string()
    } else {
        format!("/?{}", params.join("&"))
    }
}

fn without(tags: &[String], tag: &str) -> Vec<String> {
    tags.iter().filter(|t| !t.eq_ignore_ascii_case(tag)).cloned().collect()
}

fn with(tags: &[String], tag: &str) -> Vec<String> {
    if tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
        tags.to_vec()
    } else {
        let mut v = tags.to_vec();
        v.push(tag.to_string());
        v
    }
}

// ---------- list view ----------

#[derive(Debug, Clone)]
pub struct TagUrl {
    pub tag: String,
    pub url: String, // add-tag page url (or remove if already active)
}

#[derive(Debug, Clone)]
pub struct RepoCard {
    pub rel: String,
    pub name: String,
    pub forge: String,
    pub description: String,
    pub tags: Vec<TagUrl>,
    pub remote_gone: bool,
    pub remote_dead: bool,
    pub branch: String,
    pub language: String,
    pub lang_class: String,
    pub snapshot_count: usize,
    pub release_count: usize,
    pub latest_release: String, // empty = none
    pub last_archived: String,  // empty = never
    pub size_human: String,
}

#[derive(Debug, Clone)]
pub struct FolderLink {
    pub name: String,
    pub url: String,
    pub count: usize,
    pub active: bool,
    pub depth: usize,
}

#[derive(Debug, Clone)]
pub struct TagLink {
    pub tag: String,
    pub url: String,
    pub count: usize,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct ChipLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ListCtx {
    pub q: String,
    pub active_tags_csv: String,
    pub folder: String,
    pub chips: Vec<ChipLink>,
    pub folders: Vec<FolderLink>,
    pub all_folders_url: String,
    pub popular_tags: Vec<TagLink>,
    pub repos: Vec<RepoCard>,
    pub total: usize,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexT {
    pub ctx: ListCtx,
}

#[derive(Template)]
#[template(path = "fragment.html")]
pub struct FragmentT {
    pub ctx: ListCtx,
}

#[derive(Default)]
struct TreeBuilder {
    children: std::collections::BTreeMap<String, TreeBuilder>,
    repos: usize,
}

impl TreeBuilder {
    fn insert(&mut self, rel: &str) {
        let parts: Vec<&str> = rel.split('/').collect();
        let mut node = self;
        for p in &parts[..parts.len() - 1] {
            node = node.children.entry(p.to_string()).or_default();
        }
        node.repos += 1;
    }
}

fn flatten_tree(node: &TreeBuilder, path: &str, depth: usize, max_depth: usize, out: &mut Vec<(String, usize, usize)>) {
    if depth > max_depth {
        return;
    }
    for (name, child) in &node.children {
        let child_path = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
        let count = count_repos(child);
        out.push((child_path.clone(), count, depth));
        flatten_tree(child, &child_path, depth + 1, max_depth, out);
    }
}

fn count_repos(node: &TreeBuilder) -> usize {
    node.repos + node.children.values().map(count_repos).sum::<usize>()
}

pub async fn build_list_ctx(st: &Arc<AppState>, tags: &[String], q: &str, folder: &str) -> ListCtx {
    let index = st.index.read().await;
    let mut entries = index.filter(tags, (!q.is_empty()).then_some(q));
    let folder = folder.trim_matches('/');
    if !folder.is_empty() {
        entries.retain(|r| r.rel == folder || r.rel.starts_with(&format!("{folder}/")));
    }

    // repo cards
    let repos: Vec<RepoCard> = entries
        .iter()
        .map(|r| {
            let total_bytes: u64 = r
                .branch_snapshots
                .iter()
                .chain(r.releases.iter())
                .map(|e| e.sidecar.zip.bytes)
                .sum();
            RepoCard {
                rel: r.rel.clone(),
                name: r.manifest.name.clone(),
                forge: r.manifest.forge.clone(),
                description: r.manifest.description.clone().unwrap_or_default(),
                tags: r
                    .manifest
                    .tags
                    .iter()
                    .map(|t| TagUrl { tag: t.clone(), url: page_url(&with(tags, t), q, folder) })
                    .collect(),
                remote_gone: r.manifest.remote_state.as_deref() == Some("unavailable"),
                remote_dead: r.manifest.remote_state.as_deref() == Some("dead"),
                branch: r.manifest.default_branch.clone(),
                language: r.manifest.language.clone().unwrap_or_default(),
                lang_class: r.manifest.language.as_deref().map(lang_class).unwrap_or_default().to_string(),
                snapshot_count: r.branch_snapshots.len(),
                release_count: r.releases.len(),
                latest_release: r.releases.first().and_then(|e| e.sidecar.version.clone()).unwrap_or_default(),
                last_archived: r
                    .branch_snapshots
                    .first()
                    .map(|e| e.sidecar.archived_at.chars().take(10).collect())
                    .unwrap_or_else(|| "-".into()),
                size_human: human_bytes(total_bytes),
            }
        })
        .collect();

    // sidebar: folders (up to 2 levels deep)
    let mut tree = TreeBuilder::default();
    for r in &index.repos {
        tree.insert(&r.rel);
    }
    let mut flat = Vec::new();
    flatten_tree(&tree, "", 0, 2, &mut flat);
    let folders: Vec<FolderLink> = flat
        .into_iter()
        .map(|(path, count, depth)| FolderLink {
            name: path.rsplit('/').next().unwrap_or(&path).to_string(),
            url: page_url(tags, q, &path),
            count,
            active: folder == path,
            depth,
        })
        .collect();

    // sidebar: popular tags (top 30)
    let mut all_tags: Vec<(String, usize)> = index.all_tags().into_iter().collect();
    all_tags.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let popular_tags: Vec<TagLink> = all_tags
        .into_iter()
        .take(30)
        .map(|(tag, count)| {
            let active = tags.iter().any(|t| t.eq_ignore_ascii_case(&tag));
            let new_tags = if active { without(tags, &tag) } else { with(tags, &tag) };
            TagLink { url: page_url(&new_tags, q, folder), tag, count, active }
        })
        .collect();

    // active filter chips
    let mut chips = Vec::new();
    for t in tags {
        chips.push(ChipLink { label: format!("#{t}"), url: page_url(&without(tags, t), q, folder) });
    }
    if !folder.is_empty() {
        chips.push(ChipLink { label: format!("📁 {folder}"), url: page_url(tags, q, "") });
    }
    if !q.is_empty() {
        chips.push(ChipLink { label: format!("“{q}”"), url: page_url(tags, "", folder) });
    }

    let total = repos.len();
    ListCtx {
        q: q.to_string(),
        active_tags_csv: tags.join(","),
        folder: folder.to_string(),
        chips,
        folders,
        all_folders_url: page_url(&[], "", ""),
        popular_tags,
        repos,
        total,
    }
}

// ---------- detail view ----------

#[derive(Debug, Clone)]
pub struct SnapView {
    pub label: String,
    pub commit: String,
    pub commit_short: String,
    pub date: String,
    pub size_human: String,
    pub download_url: String,
    pub version: String, // empty = not a versioned release
    pub changelog_html: String, // rendered markdown release notes
}

#[derive(Debug, Clone)]
pub struct FileView {
    pub name: String,
    pub is_dir: bool,
    pub size_human: String,
    pub url: String, // empty = not viewable
}

#[derive(Debug, Clone)]
pub struct DetailCtx {
    pub rel: String,
    pub name: String,
    pub origin: String,
    pub forge: String,
    pub description: String,
    pub notes: String,
    pub tags: Vec<String>,
    pub suggested_tags: Vec<String>,
    pub unidentified: bool,
    pub stars: String,
    pub default_branch: String,
    pub language: String,
    pub lang_class: String,
    pub added: String,
    pub last_checked: String,
    pub remote_state: String,
    pub schedule_days: u32,
    pub keep_branch: i64,
    pub keep_releases: i64,
    pub branch_snapshots: Vec<SnapView>,
    pub releases: Vec<SnapView>,
    pub readme_html: String,
    pub readme_file: String,
    pub readme_truncated: bool,
    pub files: Vec<FileView>,
    pub total_size: String,
    pub back_url: String,
    pub head_commit: String, // newest snapshot's zip file (blob URLs)
}

#[derive(Template)]
#[template(path = "repo_detail.html")]
pub struct DetailT {
    pub ctx: DetailCtx,
}

#[derive(Debug, Clone)]
pub struct TagsCtx {
    pub rel: String,
    pub tags: Vec<String>,
    pub suggested_tags: Vec<String>,
}

#[derive(Template)]
#[template(path = "tags_edit.html")]
pub struct TagsT {
    pub ctx: TagsCtx,
}

#[derive(Template)]
#[template(path = "blob.html")]
pub struct BlobT {
    pub ctx: BlobCtx,
}

#[derive(Debug, Clone)]
pub struct BlobCtx {
    pub rel: String,
    pub name: String,
    pub commit: String,
    pub is_markdown: bool,
    pub content: String,
    pub truncated: bool,
    pub back_url: String,
}

#[derive(Debug, Clone)]
pub struct JobCtx {
    pub id: u64,
    pub message: String,
    pub done: bool,
    pub failed: bool,
    pub view_url: String, // empty = none
    pub oob_app: String,
}

#[derive(Template)]
#[template(path = "job_status.html")]
pub struct JobT {
    pub ctx: JobCtx,
}

/// Build the repo detail context (includes git file reads; best-effort).
pub async fn build_detail_ctx(st: &Arc<AppState>, repo: &crate::index::RepoEntry) -> DetailCtx {
    let m = &repo.manifest;
    let head = repo
        .branch_snapshots
        .first()
        .map(|e| e.sidecar.commit.clone())
        .or_else(|| repo.releases.first().map(|e| e.sidecar.commit.clone()))
        .unwrap_or_default();

    let snap_view = |e: &crate::index::SnapshotEntry| SnapView {
        label: e.sidecar.r#ref.clone(),
        commit: e.sidecar.commit.clone(),
        commit_short: e.sidecar.commit.chars().take(7).collect(),
        date: e.sidecar.archived_at.chars().take(10).collect(),
        size_human: human_bytes(e.sidecar.zip.bytes),
        download_url: format!(
            "/api/repos/{}/archive/{}/{}",
            repo.rel,
            e.dir
                .strip_prefix(&repo.dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            e.sidecar.zip.file
        ),
        version: e.sidecar.version.clone().unwrap_or_else(|| e.sidecar.r#ref.clone()),
        changelog_html: e
            .sidecar
            .changelog
            .as_deref()
            .map(crate::files::markdown_to_html)
            .unwrap_or_default(),
    };

    let branch_snapshots = repo.branch_snapshots.iter().map(snap_view).collect();
    let releases = repo.releases.iter().map(snap_view).collect();
    let total_bytes: u64 = repo
        .branch_snapshots
        .iter()
        .chain(repo.releases.iter())
        .map(|e| e.sidecar.zip.bytes)
        .sum();

    // README + root file listing, read from the newest snapshot's ZIP.
    // (The zip is the only persistent artifact; works for imported zips too.)
    let newest = repo
        .branch_snapshots
        .first()
        .or_else(|| repo.releases.first());
    let (readme_html, readme_file, readme_truncated, files) = if let Some(e) = newest {
        let zip_path = e.dir.join(&e.sidecar.zip.file);
        // plain README.md first (written at snapshot time); zip is the fallback
        let readme = match std::fs::read_to_string(repo.dir.join("README.md")) {
            Ok(text) => Some(("README.md".to_string(), text, false)),
            Err(_) => crate::files::readme_from_zip(&zip_path),
        };
        let (readme_html, readme_file, readme_truncated) = match readme {
            Some((f, text, t)) => {
                let html = crate::files::markdown_to_html(&text);
                (html, f, t)
            }
            None => (String::new(), String::new(), false),
        };
        let zip_name = e.sidecar.zip.file.clone();
        let entries = crate::files::list_zip(&zip_path).unwrap_or_default();
        let files: Vec<FileView> = entries
            .iter()
            .map(|ent| {
                let viewable = !ent.is_dir && is_viewable(&ent.name);
                FileView {
                    name: ent.name.clone(),
                    is_dir: ent.is_dir,
                    size_human: human_bytes(ent.size),
                    url: if viewable {
                        format!("/repos/{}/blob/{}/{}", repo.rel, zip_name, ent.name)
                    } else {
                        String::new()
                    },
                }
            })
            .collect();
        (readme_html, readme_file, readme_truncated, files)
    } else {
        (String::new(), String::new(), false, Vec::new())
    };

    DetailCtx {
        rel: repo.rel.clone(),
        name: m.name.clone(),
        origin: m.origin.clone().unwrap_or_default(),
        forge: m.forge.clone(),
        description: m.description.clone().unwrap_or_default(),
        notes: m.notes.clone().unwrap_or_default(),
        stars: m.stars.map(|s| format!("⭐ {s}")).unwrap_or_default(),
        unidentified: m.unidentified,
        suggested_tags: m
            .suggested_tags
            .iter()
            .filter(|t| !m.tags.iter().any(|e| e.eq_ignore_ascii_case(t)))
            .cloned()
            .collect(),
        tags: m.tags.clone(),
        default_branch: m.default_branch.clone(),
        language: m.language.clone().unwrap_or_default(),
        lang_class: m.language.as_deref().map(lang_class).unwrap_or_default().to_string(),
        added: m.added.chars().take(10).collect(),
        last_checked: m.last_checked.clone().unwrap_or_else(|| "never".into()),
        remote_state: m.remote_state.clone().unwrap_or_default(),
        schedule_days: m.schedule.interval_days,
        keep_branch: m
            .retention
            .keep_branch_snapshots
            .unwrap_or(st.cfg().await.retention.keep_branch_snapshots),
        keep_releases: m
            .retention
            .keep_releases
            .unwrap_or(st.cfg().await.retention.keep_releases),
        branch_snapshots,
        releases,
        readme_html,
        readme_file,
        readme_truncated,
        files,
        total_size: human_bytes(total_bytes),
        back_url: "/".to_string(),
        head_commit: head,
    }
}

fn is_viewable(name: &str) -> bool {
    const EXTS: &[&str] = &[
        "md", "markdown", "txt", "toml", "json", "yml", "yaml", "xml", "rs", "c", "h", "cpp", "py",
        "js", "ts", "sh", "patch", "diff", "cfg", "ini", "gitignore", "gitattributes", "ld",
        "s", "asm",
    ];
    match name.rsplit_once('.') {
        Some((_, ext)) => EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext)) || name.starts_with('.'),
        None => true, // extensionless files like LICENSE, Dockerfile, Makefile
    }
}
// ---------- import view ----------

#[derive(Debug, Clone)]
pub struct QueryLink {
    pub q: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ImportRowView {
    pub i: usize,
    pub file_name: String,
    pub size_human: String,
    pub detected_origin: String,
    pub detected_name: String,
    pub detected_description: String,
    pub readme_excerpt: String,
    pub evidence: String,
    pub origin_value: String,
    pub tags_value: String,
    pub unknown_default: bool,
    pub suggest_url: String,
}

#[derive(Debug, Clone)]
pub struct ImportRowsCtx {
    pub scan_id: u64,
    pub rows: Vec<ImportRowView>,
    pub llm_enabled: bool,
    pub count: usize,
}

#[derive(askama::Template)]
#[template(path = "import.html")]
pub struct ImportT;

#[derive(askama::Template)]
#[template(path = "import_rows.html")]
pub struct ImportRowsT {
    pub ctx: ImportRowsCtx,
}

#[derive(Debug, Clone)]
pub struct CandView {
    pub full_name: String,
    pub url: String,
    pub description: String,
    pub stars: u64,
}

#[derive(Debug, Clone)]
pub struct SuggestCtx {
    pub i: usize,
    pub scan_id: u64,
    pub query: String,
    pub candidates: Vec<CandView>,
    pub llm_name: String,
    pub llm_queries: Vec<QueryLink>,
    pub error: String,
}

#[derive(askama::Template)]
#[template(path = "import_suggest.html")]
pub struct SuggestT {
    pub ctx: SuggestCtx,
}

pub async fn build_import_ctx(st: &Arc<AppState>, scan_id: u64, scan: &crate::importer::ImportScan) -> ImportRowsCtx {
    let rows: Vec<ImportRowView> = scan
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let origin_value = r.detected_origin.clone().unwrap_or_default();
            let query = r
                .detected_name
                .clone()
                .unwrap_or_else(|| r.file_name.trim_end_matches(".zip").to_string());
            ImportRowView {
                i,
                file_name: r.file_name.clone(),
                size_human: human_bytes(r.size),
                detected_origin: origin_value.clone(),
                detected_name: r.detected_name.clone().unwrap_or_default(),
                detected_description: r.detected_description.clone().unwrap_or_default(),
                readme_excerpt: r.readme_excerpt.clone(),
                evidence: r.evidence.join(", "),
                suggest_url: format!(
                    "/import/suggest?scan={scan_id}&i={i}&q={}",
                    urlencode(&query)
                ),
                origin_value,
                tags_value: String::new(),
                unknown_default: r.detected_origin.is_none(),
            }
        })
        .collect();
    ImportRowsCtx {
        scan_id,
        count: rows.len(),
        llm_enabled: st.cfg().await.llm.enabled,
        rows,
    }
}

// ---------- settings view ----------

#[derive(Debug, Clone, Default)]
pub struct SettingsCtx {
    // retention
    pub keep_branch: String,
    pub keep_releases: String,
    // scheduler
    pub sched_enabled: bool,
    pub interval_days: String,
    pub poll_hours: String,
    pub poll_every: String,
    pub dead_after: String,
    // read-only info
    pub bind: String,
    pub archive_root: String,
    // llm
    pub llm_enabled: bool,
    pub llm_url: String,
    pub llm_model: String,
    pub llm_batch: String,
    pub disable_thinking: bool,
    pub saved: bool,
}

#[derive(askama::Template)]
#[template(path = "settings.html")]
pub struct SettingsT {
    pub ctx: SettingsCtx,
}

pub fn settings_ctx_from(cfg: &crate::config::Config, saved: bool) -> SettingsCtx {
    SettingsCtx {
        keep_branch: cfg.retention.keep_branch_snapshots.to_string(),
        keep_releases: cfg.retention.keep_releases.to_string(),
        sched_enabled: cfg.scheduler.enabled,
        interval_days: cfg.scheduler.default_interval_days.to_string(),
        poll_hours: cfg.scheduler.release_poll_hours.to_string(),
        poll_every: cfg.scheduler.poll_every_secs.to_string(),
        dead_after: cfg.scheduler.dead_after_days.to_string(),
        bind: cfg.server.bind.clone(),
        archive_root: cfg.archive.root.clone(),
        llm_enabled: cfg.llm.enabled,
        llm_url: cfg.llm.url.clone(),
        llm_model: cfg.llm.model.clone().unwrap_or_default(),
        llm_batch: cfg.llm.batch_size.to_string(),
        disable_thinking: cfg.llm.disable_thinking,
        saved,
    }
}

// ---------- login view ----------

#[derive(Debug, Clone)]
pub struct LoginCtx {
    pub error: String,
}

#[derive(askama::Template)]
#[template(path = "login.html")]
pub struct LoginT {
    pub ctx: LoginCtx,
}

// ---------- notifications list view ----------

#[derive(Debug, Clone)]
pub struct NotifView {
    pub kind: String,
    pub repo: String,
    pub title: String,
    pub at: String,
    pub changelog_html: String,
}

#[derive(Debug, Clone)]
pub struct NotificationsCtx {
    pub items: Vec<NotifView>,
    pub can_mark_read: bool,
}

#[derive(askama::Template)]
#[template(path = "notifications_list.html")]
pub struct NotificationsT {
    pub ctx: NotificationsCtx,
}
