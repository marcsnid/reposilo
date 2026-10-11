//! View-model structs and builders for the HTMX UI (askama templates).

use std::sync::Arc;

use askama::Template;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

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

/// User-selectable badge colors: (manifest key, label, CSS dot modifier).
pub const COLOR_CHOICES: &[(&str, &str, &str)] = &[
    ("red", "Red", "dot-0"),
    ("orange", "Orange", "dot-1"),
    ("yellow", "Yellow", "dot-2"),
    ("green", "Green", "dot-3"),
    ("blue", "Blue", "dot-4"),
    ("purple", "Purple", "dot-5"),
    ("pink", "Pink", "dot-6"),
    ("gray", "Gray", "dot-7"),
];

/// Map a palette key to its CSS dot class (None for empty/unknown).
pub fn color_class(key: &str) -> Option<&'static str> {
    COLOR_CHOICES.iter().find(|(k, _, _)| *k == key).map(|(_, _, c)| *c)
}

/// Hex value for a palette key, kept in sync with the `.dot-N` colors in
/// `style.css`.
const COLOR_HEX: &[(&str, &str)] = &[
    ("red", "#e5534b"),
    ("orange", "#e0823d"),
    ("yellow", "#bb8009"),
    ("green", "#57ab5a"),
    ("blue", "#3992c4"),
    ("purple", "#8250df"),
    ("pink", "#d6318c"),
    ("gray", "#768390"),
];

fn palette_hex(key: &str) -> Option<&'static str> {
    COLOR_HEX.iter().find(|(k, _)| *k == key).map(|(_, h)| *h)
}

/// Resolve the tint shown on a card/panel: the user's palette choice wins,
/// then the icon-derived color, then a stable hash so every repo still has an
/// identity color (e.g. when icons are disabled).
pub fn resolve_tint(color: Option<&str>, auto_color: Option<&str>, rel: &str) -> String {
    if let Some(hex) = color.and_then(palette_hex) {
        return hex.to_string();
    }
    if let Some(hex) = auto_color.filter(|h| !h.is_empty()) {
        return hex.to_string();
    }
    crate::color::hash_color(rel)
}

/// One swatch in the metadata color picker.
#[derive(Debug, Clone)]
pub struct ColorChoiceView {
    pub key: String,
    pub name: String,
    pub class: String,
}

pub fn color_choices() -> Vec<ColorChoiceView> {
    COLOR_CHOICES
        .iter()
        .map(|(k, n, c)| ColorChoiceView { key: k.to_string(), name: n.to_string(), class: c.to_string() })
        .collect()
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

/// A short duration label, e.g. "45s" or "2m 05s".
fn human_secs(secs: u64) -> String {
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Relative "time ago" for an RFC3339 timestamp, plus its exact `YYYY-MM-DD`
/// date (for a `title` tooltip). e.g. `("3d ago", "2026-10-03")`.
pub fn humanize_ago(ts: &str) -> Option<(String, String)> {
    humanize_ago_from(ts, OffsetDateTime::now_utc())
}

/// Deterministic core of `humanize_ago` (fixed `now`), for tests.
pub fn humanize_ago_from(ts: &str, now: OffsetDateTime) -> Option<(String, String)> {
    let t = OffsetDateTime::parse(ts, &Rfc3339).ok()?;
    let exact = format!("{:04}-{:02}-{:02}", t.year(), u8::from(t.month()), t.day());
    let secs = (now - t).whole_seconds().max(0);
    let human = if secs < 45 {
        "just now".to_string()
    } else if secs < 3600 {
        format!("{}m ago", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 86_400 * 30 {
        format!("{}d ago", secs / 86_400)
    } else if secs < 86_400 * 365 {
        format!("{}mo ago", secs / (86_400 * 30))
    } else {
        format!("{}y ago", secs / (86_400 * 365))
    };
    Some((human, exact))
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

/// How many repo cards render per page; the grid loads more on scroll.
pub const REPO_PAGE_SIZE: usize = 48;

/// URL the grid's infinite-scroll sentinel requests for the next page of
/// cards, carrying the current filters so the feed stays consistent.
fn repo_more_url(tags: &[String], q: &str, folder: &str, offset: usize) -> String {
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
    params.push(format!("offset={offset}"));
    format!("/repos/more?{}", params.join("&"))
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
    /// Origin URL (empty for unidentified imports), used by the card's
    /// "open origin" quick action.
    pub origin: String,
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
    pub last_archived: String,  // exact date (tooltip), "-" = never
    pub last_archived_ago: String, // "3d ago" / "never"
    pub size_human: String,
    pub color: String,       // palette key or empty = no tint
    pub color_class: String, // "dot-N" or empty (detail header)
    pub tint: String,        // resolved #rrggbb tint for the card
    pub has_icon: bool,      // stored owner avatar
}

#[derive(Debug, Clone)]
pub struct FolderLink {
    pub rel: String,
    pub name: String,
    pub url: String,
    pub count: usize,
    pub active: bool,
    pub depth: usize,
    /// Emoji icon from the folder manifest; empty = colored dot.
    pub icon: String,
    /// CSS class for the default colored dot (stable per folder name).
    pub dot_class: String,
    /// Left padding for unlimited nesting depth, e.g. "0.9rem".
    pub indent: String,
    /// URL of the sidebar edit form for this folder.
    pub edit_url: String,
    /// True when the folder has nested folders (show the collapse control).
    pub has_children: bool,
}

/// Encode a slash-separated archive path for use in a URL path (slashes
/// must survive; each segment is percent-encoded on its own).
pub fn path_encode(p: &str) -> String {
    p.split('/').map(urlencode).collect::<Vec<_>>().join("/")
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
    /// True when there are more repos after this page (grid loads them on scroll).
    pub has_more: bool,
    /// URL the grid's sentinel requests for the next page of cards.
    pub more_url: String,
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexT {
    pub ctx: ListCtx,
}

#[derive(Template)]
#[template(path = "repo_grid.html")]
pub struct RepoMoreT {
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
    /// Explicitly-managed folder (has folder.json) or an implicit prefix.
    explicit: bool,
    icon: Option<String>,
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

    /// Mark a folder as explicitly-managed (folder.json on disk), so it
    /// shows even when empty; may carry an icon.
    fn insert_folder(&mut self, rel: &str, icon: Option<String>) {
        let mut node = self;
        for p in rel.split('/') {
            node = node.children.entry(p.to_string()).or_default();
            node.explicit = true;
        }
        node.icon = icon;
    }
}

fn flatten_tree(
    node: &TreeBuilder,
    path: &str,
    depth: usize,
    out: &mut Vec<(String, usize, usize, Option<String>, bool)>,
) {
    for (name, child) in &node.children {
        let child_path = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
        let count = count_repos(child);
        out.push((child_path.clone(), count, depth, child.icon.clone(), child.explicit));
        flatten_tree(child, &child_path, depth + 1, out);
    }
}

fn count_repos(node: &TreeBuilder) -> usize {
    node.repos + node.children.values().map(count_repos).sum::<usize>()
}

/// Every folder path the archive knows: explicit folder.json dirs plus
/// implicit prefixes derived from repo rels (sorted, deduped).
pub fn all_folder_paths(index: &crate::index::Index) -> Vec<String> {
    let mut options = std::collections::BTreeSet::new();
    let mut add = |rel: &str, skip_last: bool| {
        let parts: Vec<&str> = rel.split('/').collect();
        let n = if skip_last { parts.len().saturating_sub(1) } else { parts.len() };
        let mut prefix = String::new();
        for part in &parts[..n] {
            prefix = if prefix.is_empty() { part.to_string() } else { format!("{prefix}/{part}") };
            options.insert(prefix.clone());
        }
    };
    for f in &index.folders {
        add(&f.rel, false);
    }
    for r in &index.repos {
        add(&r.rel, true);
    }
    options.into_iter().collect()
}

/// A path is usable as a folder if it exists in the index (explicit or
/// implicit) and doesn't run through a repo directory.
pub fn valid_folder_path(path: &str, index: &crate::index::Index) -> bool {
    if index
        .repos
        .iter()
        .any(|r| path == r.rel || path.starts_with(&format!("{}/", r.rel)))
    {
        return false;
    }
    all_folder_paths(index).iter().any(|p| p == path)
}

/// Stable color for a folder's default dot: hash of the folder path
/// (nested folders get distinct colors, same folder always the same).
fn folder_dot_color(path: &str) -> u32 {
    path.bytes().map(|b| b as u32).fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b)) % 8
}

pub async fn build_list_ctx(
    st: &Arc<AppState>,
    tags: &[String],
    q: &str,
    folder: &str,
    offset: usize,
) -> ListCtx {
    let color_on = st.cfg().await.ui.color;
    let index = st.index.read().await;
    let mut entries = index.filter(tags, (!q.is_empty()).then_some(q));
    let folder = folder.trim_matches('/');
    if !folder.is_empty() {
        entries.retain(|r| r.rel == folder || r.rel.starts_with(&format!("{folder}/")));
    }

    // repo cards: one page at a time, in the index's stable order
    let total = entries.len();
    let offset = offset.min(total);
    let end = (offset + REPO_PAGE_SIZE).min(total);
    let repos: Vec<RepoCard> = entries[offset..end]
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
                origin: r.manifest.origin.clone().unwrap_or_default(),
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
                last_archived_ago: r
                    .branch_snapshots
                    .first()
                    .and_then(|e| humanize_ago(&e.sidecar.archived_at))
                    .map(|(h, _)| h)
                    .unwrap_or_else(|| "never".into()),
                size_human: human_bytes(total_bytes),
                color: r.manifest.color.clone().unwrap_or_default(),
                color_class: r
                    .manifest
                    .color
                    .as_deref()
                    .and_then(color_class)
                    .unwrap_or_default()
                    .to_string(),
                tint: if color_on {
                    resolve_tint(
                        r.manifest.color.as_deref(),
                        r.manifest.auto_color.as_deref(),
                        &r.rel,
                    )
                } else {
                    String::new()
                },
                has_icon: r.has_icon,
            }
        })
        .collect();

    // sidebar: folders at any nesting depth, with icons
    let mut tree = TreeBuilder::default();
    for r in &index.repos {
        tree.insert(&r.rel);
    }
    for f in &index.folders {
        tree.insert_folder(&f.rel, f.manifest.icon.clone());
    }
    let mut flat = Vec::new();
    flatten_tree(&tree, "", 0, &mut flat);
    let folders: Vec<FolderLink> = flat
        .iter()
        .enumerate()
        .map(|(i, (path, count, depth, icon, _explicit))| {
            // A folder has children when the next tree entry is nested deeper.
            let has_children = flat
                .get(i + 1)
                .is_some_and(|(_, _, next_depth, _, _)| *next_depth > *depth);
            FolderLink {
                name: path.rsplit('/').next().unwrap_or(path).to_string(),
                rel: path.clone(),
                url: page_url(tags, q, path),
                count: *count,
                active: folder == path.as_str(),
                depth: *depth,
                icon: icon.clone().unwrap_or_default(),
                dot_class: format!("dot-{}", folder_dot_color(path) % 8),
                indent: format!("{:.1}rem", *depth as f32 * 0.75),
                edit_url: format!("/folders/edit?rel={}", urlencode(path)),
                has_children,
            }
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

    let has_more = end < total;
    let more_url = repo_more_url(tags, q, folder, end);
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
        has_more,
        more_url,
    }
}

// ---------- detail view ----------

#[derive(Debug, Clone)]
pub struct AssetView {
    pub name: String,
    /// Human-readable platform label ("Linux · x86_64").
    pub platform: String,
    pub size_human: String,
    pub download_url: String,
}

#[derive(Debug, Clone)]
pub struct SnapView {
    pub label: String,
    pub commit: String,
    pub commit_short: String,
    pub date: String,       // "3d ago"
    pub date_exact: String, // exact date (tooltip)
    pub size_human: String,
    pub download_url: String,
    pub version: String, // empty = not a versioned release
    pub anchor: String,  // HTML id for deep links from notifications
    pub changelog_html: String, // rendered markdown release notes
    /// Release binaries bundled with this release (empty for branch snapshots).
    pub assets: Vec<AssetView>,
}

#[derive(Debug, Clone)]
pub struct FileView {
    pub name: String,
    pub is_dir: bool,
    pub size_human: String,
    pub url: String, // empty = not viewable
}

/// One entry in the new-folder parent dropdown.
#[derive(Debug, Clone)]
pub struct FolderParentOption {
    pub path: String,
    pub selected: bool,
}

#[derive(Debug, Clone)]
pub struct FolderFormCtx {
    /// "new" or "edit"
    pub mode: String,
    /// Full rel path (edit mode; empty for new).
    pub rel: String,
    /// POST target (path-encoded).
    pub update_url: String,
    /// Folder name (last path segment).
    pub name: String,
    /// Parent folder path (empty = archive root).
    pub parent: String,
    /// Existing folders, for the parent dropdown (with the current one marked).
    pub parents: Vec<FolderParentOption>,
    /// Current icon (empty = colored dot).
    pub icon: String,
    /// Repo count inside (edit mode); 0 allows delete.
    pub count: usize,
}

#[derive(Template)]
#[template(path = "folder_form.html")]
pub struct FolderFormT {
    pub ctx: FolderFormCtx,
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
    pub folder: String,
    pub folder_options: Vec<String>,
    pub unidentified: bool,
    pub stars: String,
    pub default_branch: String,
    pub language: String,
    pub lang_class: String,
    pub added: String,           // "3d ago"
    pub added_exact: String,     // exact date (tooltip)
    pub last_checked: String,    // "2h ago" / "Never"
    pub last_checked_exact: String, // exact timestamp (tooltip)
    pub color: String,           // palette key or empty
    pub color_class: String,     // "dot-N" or empty
    pub tint: String,            // resolved #rrggbb tint for the About panel
    pub color_choices: Vec<ColorChoiceView>,
    pub has_icon: bool,          // stored owner avatar
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
    let color_on = st.cfg().await.ui.color;
    let m = &repo.manifest;
    let head = repo
        .branch_snapshots
        .first()
        .map(|e| e.sidecar.commit.clone())
        .or_else(|| repo.releases.first().map(|e| e.sidecar.commit.clone()))
        .unwrap_or_default();

    // folder the repo sits in (parent rel, empty = archive root) plus every
    // folder path the archive knows, for the move dropdown
    let folder = repo.rel.rsplit_once('/').map(|(p, _)| p.to_string()).unwrap_or_default();
    let folder_options = {
        let index = st.index.read().await;
        all_folder_paths(&index)
    };

    let snap_view = |e: &crate::index::SnapshotEntry| {
        let (date, date_exact) = humanize_ago(&e.sidecar.archived_at).unwrap_or_else(|| {
            let d: String = e.sidecar.archived_at.chars().take(10).collect();
            (d.clone(), d)
        });
        SnapView {
        label: e.sidecar.r#ref.clone(),
        commit: e.sidecar.commit.clone(),
        commit_short: e.sidecar.commit.chars().take(7).collect(),
        date,
        date_exact,
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
        assets: {
            let subdir = e
                .dir
                .strip_prefix(&repo.dir)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            e.sidecar
                .assets
                .iter()
                .map(|a| AssetView {
                    name: a.name.clone(),
                    platform: crate::platform::describe(&a.platform),
                    size_human: human_bytes(a.bytes),
                    download_url: format!(
                        "/api/repos/{}/asset/{}",
                        repo.rel,
                        path_encode(&format!("{subdir}/assets/{}", a.name))
                    ),
                })
                .collect()
        },
        anchor: e.sidecar.anchor(),
        }
    };

    let branch_snapshots = repo.branch_snapshots.iter().map(snap_view).collect();
    let releases = repo.releases.iter().map(snap_view).collect();
    let total_bytes: u64 = repo
        .branch_snapshots
        .iter()
        .chain(repo.releases.iter())
        .map(|e| {
            e.sidecar.zip.bytes + e.sidecar.assets.iter().map(|a| a.bytes).sum::<u64>()
        })
        .sum();

    // README + root file listing, read from the newest snapshot's archive
    // (zip or tar.zst; the archive is the only persistent artifact, and this
    // works for imported zips too).
    let newest = repo
        .branch_snapshots
        .first()
        .or_else(|| repo.releases.first());
    let (readme_html, readme_file, readme_truncated, files) = if let Some(e) = newest {
        let zip_path = e.dir.join(&e.sidecar.zip.file);
        // plain README.md first (written at snapshot time); archive is the fallback
        let readme = match std::fs::read_to_string(repo.dir.join("README.md")) {
            Ok(text) => Some(("README.md".to_string(), text, false)),
            Err(_) => crate::files::readme_from_archive(&zip_path),
        };
        let (readme_html, readme_file, readme_truncated) = match readme {
            Some((f, text, t)) => {
                let html = crate::files::markdown_to_html(&text);
                (html, f, t)
            }
            None => (String::new(), String::new(), false),
        };
        let zip_name = e.sidecar.zip.file.clone();
        let entries = match st.archive_index(&zip_path).await {
            Some(idx) => idx.list_root(),
            None => Vec::new(),
        };
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

    let (added, added_exact) = humanize_ago(&m.added).unwrap_or_else(|| {
        let d: String = m.added.chars().take(10).collect();
        (d.clone(), d)
    });
    let (last_checked, last_checked_exact) = m
        .last_checked
        .as_deref()
        .and_then(humanize_ago)
        .unwrap_or_else(|| ("Never".to_string(), "Never".to_string()));

    DetailCtx {
        rel: repo.rel.clone(),
        name: m.name.clone(),
        origin: m.origin.clone().unwrap_or_default(),
        forge: m.forge.clone(),
        description: m.description.clone().unwrap_or_default(),
        notes: m.notes.clone().unwrap_or_default(),
        folder,
        folder_options,
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
        added,
        added_exact,
        last_checked,
        last_checked_exact,
        color: m.color.clone().unwrap_or_default(),
        color_class: m.color.as_deref().and_then(color_class).unwrap_or_default().to_string(),
        tint: if color_on {
            resolve_tint(m.color.as_deref(), m.auto_color.as_deref(), &repo.rel)
        } else {
            String::new()
        },
        color_choices: color_choices(),
        has_icon: repo.has_icon,
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
    // scheduled integrity checks
    pub verify_enabled: bool,
    pub verify_interval_days: String,
    // read-only info
    pub bind: String,
    pub archive_root: String,
    // archive schema
    pub schema_version: u32,
    pub outdated_repos: usize,
    // llm
    pub llm_enabled: bool,
    pub llm_url: String,
    pub llm_model: String,
    pub llm_batch: String,
    pub disable_thinking: bool,
    // otel
    pub otel_enabled: bool,
    pub otel_endpoint: String,
    pub otel_service: String,
    pub otel_interval: String,
    // release binaries
    pub release_platforms: Vec<PlatformChoiceView>,
    pub release_all: bool,
    pub release_platforms_extra: String,
    pub release_max_asset_mb: String,
    // tags
    pub take_suggested_tags: bool,
    pub tags_folders: bool,
    // icons
    pub fetch_icons: bool,
    // appearance
    pub color: bool,
    // notifications
    pub webhook_url: String,
    pub notif_keep_days: String,
    // scheduler extras
    pub run_on_add: bool,
    pub max_concurrent: String,
    // llm extras
    pub llm_max_tags: String,
    pub llm_timeout: String,
    // otel extras
    pub otel_logs: bool,
    pub otel_metrics: bool,
    pub otel_traces: bool,
    // rate limiting + cache
    pub remote_enabled: bool,
    pub remote_min_interval: String,
    pub remote_jitter: String,
    pub remote_respect: bool,
    pub remote_max_wait: String,
    pub remote_max_cooldown: String,
    pub cache_conditional: bool,
    // structural, shown read only here
    pub archive_format: String,
    pub archive_zstd_level: String,
    pub git_depth: String,
    pub git_timeout: String,
    pub git_extra_args: String,
    pub github_token_set: bool,
    pub gitlab_token_set: bool,
    pub forgejo_token_set: bool,
    pub saved: bool,
}

/// One platform checkbox in Settings.
#[derive(Debug, Clone)]
pub struct PlatformChoiceView {
    pub key: String,
    pub label: String,
    pub checked: bool,
}

#[derive(askama::Template)]
#[template(path = "settings.html")]
pub struct SettingsT {
    pub ctx: SettingsCtx,
}

/// The settings form body only, for the in-place save swap. Keeping it a
/// separate template avoids nesting a full page (with `base.html`) inside the
/// form on save.
#[derive(askama::Template)]
#[template(path = "settings_form.html")]
pub struct SettingsFormT {
    pub ctx: SettingsCtx,
}

pub fn settings_ctx_from(cfg: &crate::config::Config, saved: bool, outdated_repos: usize) -> SettingsCtx {
    // canonicalize the configured release filters so the checkboxes reflect
    // whatever the user typed ("win64" shows up as Windows · x64)
    let canonical: Vec<String> = cfg
        .releases
        .platforms
        .iter()
        .filter_map(|p| crate::platform::canonical(p))
        .collect();
    let all = canonical.iter().any(|p| p == "all");
    let release_platforms = crate::platform::CHOICES
        .iter()
        .map(|c| PlatformChoiceView {
            key: c.slug.to_string(),
            label: c.label.to_string(),
            checked: all || canonical.iter().any(|p| p == c.slug),
        })
        .collect();
    let release_platforms_extra = canonical
        .iter()
        .filter(|p| p.as_str() != "all" && !crate::platform::CHOICES.iter().any(|c| c.slug == p.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    SettingsCtx {
        keep_branch: cfg.retention.keep_branch_snapshots.to_string(),
        keep_releases: cfg.retention.keep_releases.to_string(),
        sched_enabled: cfg.scheduler.enabled,
        interval_days: cfg.scheduler.default_interval_days.to_string(),
        poll_hours: cfg.scheduler.release_poll_hours.to_string(),
        poll_every: cfg.scheduler.poll_every_secs.to_string(),
        dead_after: cfg.scheduler.dead_after_days.to_string(),
        verify_enabled: cfg.verify.enabled,
        verify_interval_days: cfg.verify.interval_days.to_string(),
        bind: cfg.server.bind.clone(),
        archive_root: cfg.archive.root.clone(),
        schema_version: crate::migrations::SCHEMA_VERSION,
        outdated_repos,
        llm_enabled: cfg.llm.enabled,
        llm_url: cfg.llm.url.clone(),
        llm_model: cfg.llm.model.clone().unwrap_or_default(),
        llm_batch: cfg.llm.batch_size.to_string(),
        disable_thinking: cfg.llm.disable_thinking,
        otel_enabled: cfg.otel.enabled,
        otel_endpoint: cfg.otel.endpoint.clone(),
        otel_service: cfg.otel.service_name.clone(),
        otel_interval: cfg.otel.interval_secs.to_string(),
        release_platforms,
        release_all: all,
        release_platforms_extra,
        release_max_asset_mb: cfg.releases.max_asset_mb.to_string(),
        take_suggested_tags: cfg.tags.take_suggested,
        tags_folders: cfg.tags.folders,
        fetch_icons: cfg.fetch_icons(),
        color: cfg.ui.color,
        webhook_url: cfg.notifications.webhook_url.clone().unwrap_or_default(),
        notif_keep_days: cfg.notifications.keep_days.to_string(),
        run_on_add: cfg.scheduler.run_on_add,
        max_concurrent: cfg.scheduler.max_concurrent.to_string(),
        llm_max_tags: cfg.llm.max_tags.to_string(),
        llm_timeout: cfg.llm.request_timeout_secs.to_string(),
        otel_logs: cfg.otel.logs,
        otel_metrics: cfg.otel.metrics,
        otel_traces: cfg.otel.traces,
        remote_enabled: cfg.remote.enabled,
        remote_min_interval: cfg.remote.min_interval_ms.to_string(),
        remote_jitter: cfg.remote.jitter_ms.to_string(),
        remote_respect: cfg.remote.respect_rate_limits,
        remote_max_wait: cfg.remote.max_wait_secs.to_string(),
        remote_max_cooldown: cfg.remote.max_cooldown_secs.to_string(),
        cache_conditional: cfg.cache.conditional,
        archive_format: cfg.archive.format.clone(),
        archive_zstd_level: cfg.archive.zstd_level.to_string(),
        git_depth: cfg.git.depth.to_string(),
        git_timeout: cfg.git.timeout_secs.to_string(),
        git_extra_args: if cfg.git.extra_args.is_empty() {
            "(none)".to_string()
        } else {
            cfg.git.extra_args.join(" ")
        },
        github_token_set: cfg.github.resolved_token().is_some(),
        gitlab_token_set: cfg.gitlab.resolved_token().is_some(),
        forgejo_token_set: cfg.forgejo.resolved_token().is_some(),
        saved,
    }
}

// ---------- verify view ----------

#[derive(Debug, Clone, Default)]
pub struct VerifyCtx {
    pub id: u64,
    pub done: bool,
    pub failed: bool,
    pub error: String,
    pub done_count: usize,
    pub total: usize,
    pub current: String,
    pub snapshots: usize,
    pub problems: Vec<VerifyProblemView>,
}

#[derive(Debug, Clone)]
pub struct VerifyProblemView {
    pub repo: String,
    pub file: String,
    pub kind: String,
    pub detail: String,
}

#[derive(askama::Template)]
#[template(path = "verify_status.html")]
pub struct VerifyT {
    pub ctx: VerifyCtx,
}

pub fn verify_ctx_running(id: u64) -> VerifyCtx {
    VerifyCtx { id, ..Default::default() }
}

pub async fn verify_ctx_from(st: &Arc<AppState>, id: u64) -> VerifyCtx {
    let (done, failed, error) = match st.job(id).await {
        Some(j) => (
            j.status != "running",
            j.status == "failed",
            j.error.unwrap_or_default(),
        ),
        None => (true, true, "unknown verify job".to_string()),
    };
    let state = st.verify_job(id).await.unwrap_or_default();
    let mut ctx = VerifyCtx {
        id,
        done,
        failed,
        error,
        done_count: state.progress.done,
        total: state.progress.total,
        current: state.progress.current,
        ..Default::default()
    };
    if let Some(report) = state.report {
        ctx.snapshots = report.snapshots;
        ctx.problems = report
            .problems
            .iter()
            .map(|p| VerifyProblemView {
                repo: p.repo.clone(),
                file: p.file.clone(),
                kind: p.kind.clone(),
                detail: p.detail.clone(),
            })
            .collect();
    }
    ctx
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
    pub kind_label: String,
    pub repo: String,
    pub repo_name: String,
    /// Link target: the exact snapshot anchor, or the repo when there is none.
    pub url: String,
    pub title: String,
    pub at: String,       // "3d ago"
    pub at_exact: String, // exact date/time (tooltip)
    pub changelog_html: String,
    pub tint: String,    // resolved #rrggbb, matches the repo's card
    pub has_icon: bool,  // stored owner avatar, shown on the left
    /// Draw the "older than a week ago" divider before this row.
    pub show_week_divider: bool,
}

/// Short, human label for a notification kind, shown as a text badge.
pub fn notif_kind_label(kind: &str) -> String {
    match kind {
        "new_release" => "release".to_string(),
        "new_snapshot" => "snapshot".to_string(),
        "remote_gone" => "remote gone".to_string(),
        "repo_added" => "added".to_string(),
        "import" => "import".to_string(),
        "autotag" => "autotag".to_string(),
        "migrate" => "migration".to_string(),
        "corrupt" => "corrupt".to_string(),
        other => other.replace('_', " "),
    }
}

#[derive(Debug, Clone)]
pub struct NotifTab {
    pub key: String,
    pub label: String,
    pub url: String,
    pub active: bool,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct NotificationsCtx {
    pub items: Vec<NotifView>,
    /// True when older notifications remain (the list loads them on scroll).
    pub has_more: bool,
    /// URL the list's sentinel requests for the next page.
    pub more_url: String,
    /// Active kind filter ("" for all).
    pub filter: String,
    /// Kind filter tabs with their counts.
    pub tabs: Vec<NotifTab>,
}

#[derive(askama::Template)]
#[template(path = "notifications_list.html")]
pub struct NotificationsT {
    pub ctx: NotificationsCtx,
}

#[derive(askama::Template)]
#[template(path = "notif_rows.html")]
pub struct NotifRowsT {
    pub ctx: NotificationsCtx,
}

/// How many notifications render per page; the list loads older ones on scroll.
pub const NOTIF_PAGE_SIZE: usize = 50;

/// Normalize a `?kind=` value to one of the known filter keys.
pub fn normalize_notif_filter(kind: &str) -> &'static str {
    match kind {
        "release" => "release",
        "snapshot" => "snapshot",
        "error" => "error",
        _ => "",
    }
}

/// True when a notification kind belongs to the given filter.
fn notif_matches(filter: &str, kind: &str) -> bool {
    match filter {
        "release" => kind == "new_release",
        "snapshot" => kind == "new_snapshot",
        "error" => matches!(kind, "remote_gone" | "corrupt"),
        _ => true,
    }
}

/// The boundary between "this week" and older. Passing the cutoff through the
/// scroll URL keeps the divider and the tab counts stable while paging.
fn notif_week_cutoff(week: Option<&str>) -> (OffsetDateTime, String) {
    if let Some(c) = week.and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()) {
        let text = c.format(&Rfc3339).unwrap_or_default();
        return (c, text);
    }
    let cutoff = OffsetDateTime::now_utc() - time::Duration::days(7);
    let text = cutoff.format(&Rfc3339).unwrap_or_default();
    (cutoff, text)
}

/// True when a notification falls inside the current week window.
fn notif_recent(at: &str, cutoff: OffsetDateTime) -> bool {
    OffsetDateTime::parse(at, &Rfc3339).map(|t| t >= cutoff).unwrap_or(false)
}

fn notif_tabs(
    all: &[crate::server::Notification],
    active: &str,
    cutoff: OffsetDateTime,
) -> Vec<NotifTab> {
    let count = |filter: &str| {
        all.iter()
            .filter(|n| notif_matches(filter, &n.kind) && notif_recent(&n.at, cutoff))
            .count()
    };
    [("", "All"), ("release", "Releases"), ("snapshot", "Snapshots"), ("error", "Errors")]
        .iter()
        .map(|(key, label)| {
            let url = if key.is_empty() {
                "/notifications".to_string()
            } else {
                format!("/notifications?kind={key}")
            };
            NotifTab {
                key: (*key).to_string(),
                label: (*label).to_string(),
                url,
                active: *key == active,
                count: count(key),
            }
        })
        .collect()
}

/// Build one page of the notification list. Retention is time-based (see
/// `notifications.keep_days`), so the store only holds recent alerts.
/// `filter` is a normalized kind filter.
pub async fn build_notifications_ctx(
    st: &Arc<AppState>,
    offset: usize,
    filter: &str,
    week: Option<&str>,
) -> NotificationsCtx {
    let color_on = st.cfg().await.ui.color;
    let all: Vec<crate::server::Notification> = st.notifications.lock().await.items.to_vec();
    let (cutoff, week_text) = notif_week_cutoff(week);
    let tabs = notif_tabs(&all, filter, cutoff);

    // Resolve each notification's repo tint, icon and display name once, so the
    // marker and avatar on the left match the repo's card.
    let lookup: std::collections::HashMap<String, (String, bool, String)> = {
        let idx = st.index.read().await;
        idx.repos
            .iter()
            .map(|r| {
                let tint = resolve_tint(
                    r.manifest.color.as_deref(),
                    r.manifest.auto_color.as_deref(),
                    &r.rel,
                );
                (r.rel.clone(), (tint, r.has_icon, r.manifest.name.clone()))
            })
            .collect()
    };

    let filtered: Vec<&crate::server::Notification> = all
        .iter()
        .filter(|n| notif_matches(filter, &n.kind))
        .collect();
    let total = filtered.len();
    // The divider goes right before the first item older than the week window,
    // and only when at least one newer item precedes it.
    let boundary = filtered.iter().position(|n| !notif_recent(&n.at, cutoff));
    let show_divider = boundary.is_some_and(|b| b > 0);
    let offset = offset.min(total);
    let end = (offset + NOTIF_PAGE_SIZE).min(total);
    let views: Vec<NotifView> = filtered[offset..end]
        .iter()
        .copied()
        .enumerate()
        .map(|(i, n)| {
            let (at, at_exact) = humanize_ago(&n.at).unwrap_or_else(|| {
                let t: String = n.at.chars().take(16).collect();
                (t.clone(), t)
            });
            let (tint, has_icon, repo_name) = match lookup.get(&n.repo) {
                Some((tint, has_icon, name)) => (tint.clone(), *has_icon, name.clone()),
                // The repo may be gone: keep a stable color from its path.
                None if !n.repo.is_empty() => (resolve_tint(None, None, &n.repo), false, n.repo.clone()),
                None => (String::new(), false, String::new()),
            };
            let tint = if color_on { tint } else { String::new() };
            let url = if n.repo.is_empty() {
                String::new()
            } else {
                match n.target.as_deref().filter(|t| !t.is_empty()) {
                    Some(target) => format!("/repos/{}#{target}", n.repo),
                    None => format!("/repos/{}", n.repo),
                }
            };
            NotifView {
                kind: n.kind.clone(),
                kind_label: notif_kind_label(&n.kind),
                repo: n.repo.clone(),
                repo_name,
                url,
                title: n.title.clone(),
                at,
                at_exact,
                changelog_html: n
                    .body
                    .as_deref()
                    .map(crate::files::markdown_to_html)
                    .unwrap_or_default(),
                tint,
                has_icon,
                show_week_divider: show_divider && boundary == Some(offset + i),
            }
        })
        .collect();

    let has_more = end < total;
    let mut params = Vec::new();
    if !filter.is_empty() {
        params.push(format!("kind={filter}"));
    }
    params.push(format!("offset={end}"));
    params.push(format!("week={}", urlencode(&week_text)));
    let more_url = format!("/notifications/more?{}", params.join("&"));
    NotificationsCtx { items: views, has_more, more_url, filter: filter.to_string(), tabs }
}

// ---------- stats view ----------

#[derive(Debug, Clone)]
pub struct DayView {
    pub date: String,      // short label, e.g. "09-24"
    pub full_date: String, // "2025-09-24"
    pub ok: u64,
    pub fail: u64,
    pub releases: u64,
    pub snapshots: u64,
    pub adds: u64,
    pub adds_fail: u64,
    pub remote_gone: u64,
    pub verify_runs: u64,
    pub verify_problems: u64,
    pub ok_h: u32,   // 0..100 for the chart
    pub fail_h: u32, // 0..100
}

/// One row of the largest-repos storage table.
#[derive(Debug, Clone)]
pub struct StatRepoView {
    pub rel: String,
    pub name: String,
    pub size_human: String,
}

/// A host currently paused by a rate-limit signal.
#[derive(Debug, Clone)]
pub struct HostLimitView {
    pub host: String,
    pub paused: String,
}

#[derive(Debug, Clone)]
pub struct StatsCtx {
    pub started_at: String,
    pub repos: u64,
    pub snapshots: u64,
    pub releases: u64,
    pub dead: u64,
    pub unavailable: u64,
    pub untagged: u64,
    pub refresh_ok: u64,
    pub refresh_fail: u64,
    pub add_ok: u64,
    pub add_fail: u64,
    pub new_releases: u64,
    pub new_snapshots: u64,
    pub remote_gone: u64,
    pub verify_runs: u64,
    pub verify_problems: u64,
    pub verify_fail: u64,
    pub last_verified_at: String,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub archive_size: String,
    pub snapshot_size: String,
    pub release_size: String,
    pub asset_size: String,
    pub largest_repos: Vec<StatRepoView>,
    pub rate_limited: u64,
    pub requests_skipped: u64,
    pub paused_hosts: Vec<HostLimitView>,
    pub success_pct: String,
    pub week: Vec<DayView>,
    pub days: Vec<DayView>,
    pub otel_enabled: bool,
    pub otel_endpoint: String,
}

#[derive(askama::Template)]
#[template(path = "stats.html")]
pub struct StatsT {
    pub ctx: StatsCtx,
}

pub fn stats_ctx_from(
    metrics: &crate::metrics::Metrics,
    totals: &crate::metrics::Totals,
    cfg: &crate::config::Config,
) -> StatsCtx {
    use time::format_description::well_known::Rfc3339;
    use time::macros::format_description;
    let long = format_description!("[year]-[month]-[day]");
    let short = format_description!("[month]-[day]");
    let now = time::OffsetDateTime::now_utc();

    let day_view = |date: String, s: &crate::metrics::DayStats, max: u64| -> DayView {
        let pct = |v: u64| -> u32 {
            if max == 0 {
                0
            } else {
                ((v as f64 / max as f64) * 100.0).round() as u32
            }
        };
        let short_date = time::OffsetDateTime::parse(&format!("{date}T00:00:00Z"), &Rfc3339)
            .ok()
            .and_then(|t| t.format(&short).ok())
            .unwrap_or_else(|| date.clone());
        DayView {
            date: short_date,
            full_date: date,
            ok: s.refresh_ok,
            fail: s.refresh_fail,
            releases: s.new_releases,
            snapshots: s.new_snapshots,
            adds: s.add_ok,
            adds_fail: s.add_fail,
            remote_gone: s.remote_gone,
            verify_runs: s.verify_runs,
            verify_problems: s.verify_problems,
            ok_h: pct(s.refresh_ok),
            fail_h: pct(s.refresh_fail),
        }
    };

    // last 7 days (zero days included), ascending, for the chart
    let zero = crate::metrics::DayStats::default();
    let mut week_raw: Vec<(String, crate::metrics::DayStats)> = Vec::new();
    for i in (0..7i64).rev() {
        let key = (now - time::Duration::days(i)).format(&long).unwrap_or_default();
        let s = metrics.days.get(&key).cloned().unwrap_or_else(|| zero.clone());
        week_raw.push((key, s));
    }
    let wmax = week_raw.iter().map(|(_, s)| s.refresh_ok.max(s.refresh_fail)).max().unwrap_or(0);
    let week: Vec<DayView> = week_raw.iter().map(|(k, s)| day_view(k.clone(), s, wmax)).collect();

    // retained days with any activity, newest first, for the table
    let dmax = metrics.days.values().map(|s| s.refresh_ok.max(s.refresh_fail)).max().unwrap_or(0);
    let mut days: Vec<DayView> = metrics
        .days
        .iter()
        .rev()
        .filter(|(_, s)| {
            s.refresh_ok + s.refresh_fail + s.add_ok + s.add_fail + s.new_releases + s.new_snapshots + s.remote_gone
                + s.verify_runs + s.verify_problems + s.verify_fail
                > 0
        })
        .map(|(k, s)| day_view(k.clone(), s, dmax))
        .collect();
    days.truncate(30);

    let sums = metrics.sums();
    let total_ops = sums.refresh_ok + sums.refresh_fail;
    let success_pct = if total_ops == 0 {
        "-".to_string()
    } else {
        format!("{:.1}%", (sums.refresh_ok as f64 / total_ops as f64) * 100.0)
    };

    StatsCtx {
        started_at: metrics.started_at.clone(),
        repos: totals.repos,
        snapshots: totals.snapshots,
        releases: totals.releases,
        dead: totals.dead,
        unavailable: totals.unavailable,
        untagged: totals.untagged,
        refresh_ok: sums.refresh_ok,
        refresh_fail: sums.refresh_fail,
        add_ok: sums.add_ok,
        add_fail: sums.add_fail,
        new_releases: sums.new_releases,
        new_snapshots: sums.new_snapshots,
        remote_gone: sums.remote_gone,
        verify_runs: sums.verify_runs,
        verify_problems: sums.verify_problems,
        verify_fail: sums.verify_fail,
        last_verified_at: metrics.last_verified_at.clone().unwrap_or_default(),
        cache_hits: totals.cache_hits,
        cache_misses: totals.cache_misses,
        archive_size: human_bytes(totals.archive_bytes),
        snapshot_size: human_bytes(totals.snapshot_bytes),
        release_size: human_bytes(totals.release_bytes),
        asset_size: human_bytes(totals.asset_bytes),
        largest_repos: totals
            .largest_repos
            .iter()
            .map(|r| StatRepoView {
                rel: r.rel.clone(),
                name: if r.name.trim().is_empty() { r.rel.clone() } else { r.name.clone() },
                size_human: human_bytes(r.bytes),
            })
            .collect(),
        rate_limited: totals.rate_limited,
        requests_skipped: totals.requests_skipped,
        paused_hosts: totals
            .paused_hosts
            .iter()
            .map(|h| HostLimitView {
                host: h.host.clone(),
                paused: human_secs(h.paused_for_secs),
            })
            .collect(),
        success_pct,
        week,
        days,
        otel_enabled: cfg.otel.enabled,
        otel_endpoint: cfg.otel.endpoint.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::{humanize_ago_from, resolve_tint, settings_ctx_from, stats_ctx_from};
    use time::macros::datetime;

    #[test]
    fn user_color_wins_over_auto_color() {
        assert_eq!(resolve_tint(Some("blue"), Some("#ff00ff"), "a/b"), "#3992c4");
    }

    #[test]
    fn auto_color_is_used_when_no_user_color() {
        assert_eq!(resolve_tint(None, Some("#ff00ff"), "a/b"), "#ff00ff");
    }

    #[test]
    fn empty_auto_color_falls_back_to_a_stable_hash() {
        let a = resolve_tint(None, Some(""), "a/b");
        assert_eq!(a, resolve_tint(None, None, "a/b"));
        assert!(a.starts_with('#') && a.len() == 7, "expected a hex tint, got {a}");
    }

    #[test]
    fn settings_ctx_includes_verify_toggle() {
        let mut cfg = crate::config::Config::default();
        let ctx = settings_ctx_from(&cfg, false, 0);
        assert!(!ctx.verify_enabled, "scheduled checks default to off");
        assert_eq!(ctx.verify_interval_days, "7");

        cfg.verify.enabled = true;
        cfg.verify.interval_days = 30;
        let ctx = settings_ctx_from(&cfg, false, 0);
        assert!(ctx.verify_enabled);
        assert_eq!(ctx.verify_interval_days, "30");
    }

    #[test]
    fn stats_ctx_includes_verify_metrics() {
        let mut m = crate::metrics::Metrics::default();
        m.bump("verify_runs");
        m.bump_by("verify_problems", 2);
        m.last_verified_at = Some("2025-01-02T00:00:00Z".into());
        let ctx = stats_ctx_from(
            &m,
            &crate::metrics::Totals::default(),
            &crate::config::Config::default(),
        );
        assert_eq!(ctx.verify_runs, 1);
        assert_eq!(ctx.verify_problems, 2);
        assert_eq!(ctx.last_verified_at, "2025-01-02T00:00:00Z");
    }

    #[test]
    fn stats_ctx_includes_storage_and_rate_limits() {
        let m = crate::metrics::Metrics::default();
        let totals = crate::metrics::Totals {
            archive_bytes: 3 * 1024 * 1024,
            snapshot_bytes: 2 * 1024 * 1024,
            release_bytes: 1024 * 1024,
            asset_bytes: 0,
            largest_repos: vec![crate::metrics::RepoSize {
                rel: "tools/ripgrep".into(),
                name: "ripgrep".into(),
                bytes: 3 * 1024 * 1024,
            }],
            rate_limited: 4,
            requests_skipped: 2,
            paused_hosts: vec![crate::metrics::HostLimit {
                host: "api.github.com".into(),
                paused_for_secs: 125,
            }],
            ..Default::default()
        };
        let ctx = stats_ctx_from(&m, &totals, &crate::config::Config::default());
        assert_eq!(ctx.archive_size, "3.0 MB");
        assert_eq!(ctx.snapshot_size, "2.0 MB");
        assert_eq!(ctx.release_size, "1.0 MB");
        assert_eq!(ctx.largest_repos.len(), 1);
        assert_eq!(ctx.largest_repos[0].name, "ripgrep");
        assert_eq!(ctx.largest_repos[0].size_human, "3.0 MB");
        assert_eq!(ctx.rate_limited, 4);
        assert_eq!(ctx.requests_skipped, 2);
        assert_eq!(ctx.paused_hosts[0].host, "api.github.com");
        assert_eq!(ctx.paused_hosts[0].paused, "2m 05s");
    }

    #[test]
    fn humanizes_relative_time() {
        let now = datetime!(2026-10-06 12:00:00 UTC);
        assert_eq!(humanize_ago_from("2026-10-06T12:00:00Z", now).unwrap().0, "just now");
        assert_eq!(humanize_ago_from("2026-10-06T11:30:00Z", now).unwrap().0, "30m ago");
        assert_eq!(humanize_ago_from("2026-10-06T09:00:00Z", now).unwrap().0, "3h ago");
        assert_eq!(humanize_ago_from("2026-10-03T12:00:00Z", now).unwrap().0, "3d ago");
        assert_eq!(humanize_ago_from("2026-08-06T12:00:00Z", now).unwrap().0, "2mo ago");
        assert_eq!(humanize_ago_from("2024-10-06T12:00:00Z", now).unwrap().0, "2y ago");
    }

    #[test]
    fn humanize_returns_exact_date() {
        let now = datetime!(2026-10-06 12:00:00 UTC);
        let (human, exact) = humanize_ago_from("2026-10-03T08:15:00Z", now).unwrap();
        assert_eq!(human, "3d ago");
        assert_eq!(exact, "2026-10-03");
    }

    #[test]
    fn humanize_rejects_garbage() {
        assert!(humanize_ago_from("not a date", datetime!(2026-10-06 12:00:00 UTC)).is_none());
    }
}
