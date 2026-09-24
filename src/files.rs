//! Read files out of a shallow.git repo via `git ls-tree` / `git show`.
//! Used by the web UI (README rendering, file browsing) and by the archiver
//! (auto-deriving a description from the README at add time).

use std::path::Path;

use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
}

async fn git_str(args: &[&str], shallow: &Path) -> Result<String> {
    let out = Command::new("git")
        .current_dir(shallow)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/true")
        .env("SSH_ASKPASS", "/usr/bin/true")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve any rev (branch name, tag, HEAD) to a full hex commit sha.
pub async fn rev_parse(shallow: &Path, rev: &str) -> Result<String> {
    anyhow::ensure!(!rev.is_empty() && !rev.contains(':') && !rev.contains(".."), "invalid rev");
    git_str(&["rev-parse", rev], shallow).await
}

async fn git_bytes(args: &[&str], shallow: &Path) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(shallow)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "/usr/bin/true")
        .env("SSH_ASKPASS", "/usr/bin/true")
        .args(args)
        .output()
        .await
        .with_context(|| format!("failed to spawn git {args:?}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// List the root tree of a commit. Directories first, then files, by name.
pub async fn list_root(shallow: &Path, commit: &str) -> Result<Vec<FileEntry>> {
    let out = git_str(&["ls-tree", "-l", "--full-name", commit], shallow).await?;
    let mut entries = Vec::new();
    for line in out.lines() {
        // <mode> <type> <object> <size>\t<path>
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 4 {
            continue;
        }
        let is_dir = parts[1] == "tree";
        let size = parts[3].parse::<u64>().unwrap_or(0);
        entries.push(FileEntry { path: path.to_string(), is_dir, size });
    }
    Ok(entries)
}

fn is_hex_sha(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_safe_path(s: &str) -> bool {
    !s.is_empty()
        && !s.contains('/')
        && !s.contains("..")
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_')
}

/// Read one file at a commit. `path` must be a simple root-level name and
/// `commit` must be a hex sha (validated to keep `git show <rev>:<path>`
/// unambiguous and safe).
pub async fn read_file(shallow: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    anyhow::ensure!(is_hex_sha(commit), "invalid commit id");
    anyhow::ensure!(is_safe_path(path), "invalid file path");
    let spec = format!("{commit}:{path}");
    git_bytes(&["show", &spec], shallow).await
}

/// Pick the README entry from a root listing, GitHub-style priority:
/// .md > .markdown > .txt > bare, regardless of listing order.
pub fn pick_readme(entries: &[FileEntry]) -> Option<&FileEntry> {
    const CANDIDATES: &[&str] = &["readme.md", "readme.markdown", "readme.txt", "readme"];
    CANDIDATES
        .iter()
        .find_map(|c| entries.iter().filter(|e| !e.is_dir).find(|e| e.path.eq_ignore_ascii_case(c)))
}

/// Render markdown to HTML.
pub fn markdown_to_html(md: &str) -> String {
    use pulldown_cmark::{html, Options, Parser};
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// First meaningful paragraph of a markdown doc (skipping headings), capped.
/// Used to auto-populate repo descriptions at archive time.
pub fn first_paragraph(md: &str) -> Option<String> {
    let mut para: Vec<&str> = Vec::new();
    for line in md.lines() {
        let t = line.trim();
        if t.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if t.starts_with('#') || t.starts_with("```") || t.starts_with('|') {
            continue; // headings, code fences, tables
        }
        if para.is_empty() && (t.starts_with("[!") || t.starts_with('<')) {
            continue; // badges / html
        }
        para.push(t);
        if para.len() >= 4 {
            break;
        }
    }
    if para.is_empty() {
        return None;
    }
    let joined: String = para.join(" ");
    let joined: String = joined.chars().take(160).collect();
    let _ = is_hex_sha; // referenced to keep helper visible in tests
    Some(joined)
}

/// Full README handling: returns (file name, rendered html, truncated).
pub async fn render_readme(shallow: &Path, commit: &str) -> Option<(String, String, bool)> {
    let entries = list_root(shallow, commit).await.ok()?;
    let readme = pick_readme(&entries)?;
    let bytes = read_file(shallow, commit, &readme.path).await.ok()?;
    const MAX: usize = 256 * 1024;
    let truncated = bytes.len() > MAX;
    let md = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)]).into_owned();
    let html = markdown_to_html(&md);
    Some((readme.path.clone(), html, truncated))
}

/// Extract the section of a CHANGELOG covering a specific version/tag.
/// Matches a markdown heading (any level) containing the version or tag, then
/// collects everything until the next heading of the same or higher level.
pub fn extract_changelog_section(text: &str, tag: &str, version: &str) -> Option<String> {
    let level = |line: &str| -> usize { line.chars().take_while(|c| *c == '#').count() };
    let mut matched_level: usize = 0;
    let mut collecting = false;
    let mut out = String::new();
    for line in text.lines() {
        let l = level(line);
        if l > 0 {
            if collecting && l <= matched_level {
                break; // next section at same/higher level
            }
            if !collecting && (line.contains(tag) || line.contains(version)) && tag.len() >= 2 {
                collecting = true;
                matched_level = l;
                out.push_str(line);
                out.push('\n');
                continue;
            }
        }
        if collecting {
            out.push_str(line);
            out.push('\n');
        }
    }
    (!out.trim().is_empty()).then_some(out)
}

/// Raw README text (up to `max` chars) for LLM context.
pub async fn readme_text(shallow: &Path, rev: &str, max: usize) -> Option<String> {
    let commit = rev_parse(shallow, rev).await.ok()?;
    let entries = list_root(shallow, &commit).await.ok()?;
    let readme = pick_readme(&entries)?;
    let bytes = read_file(shallow, &commit, &readme.path).await.ok()?;
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(max)]).into_owned();
    Some(text)
}

/// Best-effort description from the README at a rev (used by `add`).
/// The rev may be a branch name, so it is resolved to a sha first.
pub async fn readme_description(shallow: &Path, rev: &str) -> Option<String> {
    let commit = rev_parse(shallow, rev).await.ok()?;
    let entries = list_root(shallow, &commit).await.ok()?;
    let readme = pick_readme(&entries)?;
    let bytes = read_file(shallow, &commit, &readme.path).await.ok()?;
    let md = String::from_utf8_lossy(&bytes[..bytes.len().min(64 * 1024)]).into_owned();
    first_paragraph(&md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_paragraph_skips_headings_and_badges() {
        let md = "# Title\n\n[![build](x)](y)\n\nThis is a **sm64** decompilation.\nMore here.\n\nLater para.";
        assert_eq!(first_paragraph(md).as_deref(), Some("This is a **sm64** decompilation. More here."));
    }

    #[test]
    fn first_paragraph_handles_no_heading() {
        assert_eq!(first_paragraph("just a line").as_deref(), Some("just a line"));
        assert_eq!(first_paragraph(""), None);
    }

    #[test]
    fn markdown_renders_headings() {
        assert!(markdown_to_html("# Hi\n").contains("<h1>Hi</h1>"));
    }

    #[test]
    fn pick_readme_prefers_md_case_insensitive() {
        let e = vec![
            FileEntry { path: "Readme.TXT".into(), is_dir: false, size: 1 },
            FileEntry { path: "readme.md".into(), is_dir: false, size: 1 },
        ];
        assert_eq!(pick_readme(&e).unwrap().path, "readme.md");
    }
}
// ---------- primary language detection ----------

fn language_of_ext(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "rs" => "Rust",
        "c" | "h" => "C",
        "cpp" | "cc" | "cxx" | "hpp" => "C++",
        "java" => "Java",
        "py" => "Python",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "go" => "Go",
        "rb" => "Ruby",
        "php" => "PHP",
        "cs" => "C#",
        "swift" => "Swift",
        "kt" | "kts" => "Kotlin",
        "scala" => "Scala",
        "sh" | "bash" => "Shell",
        "lua" => "Lua",
        "pl" => "Perl",
        "jl" => "Julia",
        "zig" => "Zig",
        "nim" => "Nim",
        "hs" => "Haskell",
        "ml" | "mli" => "OCaml",
        "ex" | "exs" => "Elixir",
        "erl" => "Erlang",
        "dart" => "Dart",
        "m" => "Objective-C",
        "mm" => "Objective-C++",
        "asm" | "s" => "Assembly",
        "vue" => "Vue",
        "html" | "htm" => "HTML",
        "css" => "CSS",
        "scss" | "sass" => "SCSS",
        "r" => "R",
        "sql" => "SQL",
        _ => return None, // md/txt/json/toml/images etc. are not languages
    })
}

const SKIP_DIRS: &[&str] = &[
    "node_modules", "vendor", "dist", "build", "target", ".git", "third_party", "deps", "extern",
];

fn in_skipped_dir(path: &str) -> bool {
    path.split('/').any(|s| SKIP_DIRS.contains(&s))
}

/// Byte-weighted primary language (linguist-lite). `size` may be 1 for
/// count-based detection (e.g. imports, where sizes aren't free).
pub fn detect_language(entries: &[(String, u64)]) -> Option<String> {
    let mut totals: std::collections::BTreeMap<&'static str, u64> = Default::default();
    for (path, size) in entries {
        if in_skipped_dir(path) {
            continue;
        }
        let ext = path
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase());
        if let Some(lang) = ext.as_deref().and_then(language_of_ext) {
            *totals.entry(lang).or_insert(0) += (*size).max(1);
        }
    }
    totals
        .into_iter()
        .max_by_key(|(_, bytes)| *bytes)
        .map(|(lang, _)| lang.to_string())
}

/// Recursive file listing of a commit: (path, size). One git call, no
/// working tree, no decompression.
pub async fn ls_tree(shallow: &Path, commit: &str) -> Result<Vec<(String, u64)>> {
    let out = git_str(&["ls-tree", "-r", "-l", "--full-name", commit], shallow).await?;
    let mut v = Vec::new();
    for line in out.lines() {
        // <mode> <type> <object> <size>\t<path>
        let Some((meta, path)) = line.split_once('\t') else { continue };
        let parts: Vec<&str> = meta.split_whitespace().collect();
        if parts.len() < 4 || parts[1] == "tree" {
            continue;
        }
        let size = parts[3].parse::<u64>().unwrap_or(1);
        v.push((path.to_string(), size));
    }
    Ok(v)
}

#[cfg(test)]
mod language_tests {
    use super::*;

    #[test]
    fn detects_rust_from_mixed_tree() {
        let entries = vec![
            ("README.md".into(), 2000),
            ("src/main.rs".into(), 5000),
            ("src/lib.rs".into(), 3000),
            ("docs/notes.txt".into(), 99999),
        ];
        assert_eq!(detect_language(&entries).as_deref(), Some("Rust"));
    }

    #[test]
    fn byte_weighted_winner() {
        let entries = vec![
            ("main.py".into(), 100),
            ("util.py".into(), 100),
            ("app.js".into(), 900), // js wins on bytes despite fewer files
        ];
        assert_eq!(detect_language(&entries).as_deref(), Some("JavaScript"));
    }

    #[test]
    fn skips_vendored_dirs() {
        let entries = vec![
            ("index.js".into(), 100),
            ("node_modules/big/big.js".into(), 1_000_000),
        ];
        assert_eq!(detect_language(&entries).as_deref(), Some("JavaScript"));
    }

    #[test]
    fn no_code_no_language() {
        let entries = vec![("README.md".into(), 10), ("data.json".into(), 10)];
        assert_eq!(detect_language(&entries), None);
    }

    #[test]
    fn count_based_via_unit_sizes() {
        let entries: Vec<(String, u64)> = ["a.py", "b.py", "c.rs"]
            .iter()
            .map(|p| (p.to_string(), 1))
            .collect();
        assert_eq!(detect_language(&entries).as_deref(), Some("Python"));
    }
}

// ---------- archive browsing (zip OR tar.zst; format detected by extension) ----------

/// List a snapshot archive's root entries (format auto-detected).
/// Directories first, then files, case-insensitive by name.
pub fn list_archive(path: &Path) -> Option<Vec<ZipEntryView>> {
    match fmt_of(path)? {
        ArchFmt::Zip => list_zip(path),
        ArchFmt::TarZst => list_tar_zst(path),
    }
}

/// Locate the archive entry whose stripped name equals `file`
/// (central directory / header listing is the traversal guard).
pub fn find_archive_entry(path: &Path, file: &str) -> Option<String> {
    match fmt_of(path)? {
        ArchFmt::Zip => zip_find_entry(path, file),
        ArchFmt::TarZst => tar_find_entry(path, file),
    }
}

/// Read one entry (exact archive name), capped at `max` bytes.
pub fn read_archive(path: &Path, entry: &str, max: usize) -> Option<Vec<u8>> {
    match fmt_of(path)? {
        ArchFmt::Zip => zip_read(path, entry, max),
        ArchFmt::TarZst => tar_read_entry(path, entry, max),
    }
}

/// Root README of an archive: (file name, raw text, truncated).
pub fn readme_from_archive(path: &Path) -> Option<(String, String, bool)> {
    match fmt_of(path)? {
        ArchFmt::Zip => readme_from_zip(path),
        ArchFmt::TarZst => readme_from_tar_zst(path),
    }
}

pub enum ArchFmt { Zip, TarZst }

fn fmt_of(path: &Path) -> Option<ArchFmt> {
    let name = path.to_string_lossy().to_lowercase();
    if name.ends_with(".tar.zst") {
        Some(ArchFmt::TarZst)
    } else if name.ends_with(".zip") {
        Some(ArchFmt::Zip)
    } else {
        None
    }
}

// ---------- tar.zst reading (zstd streaming + tar headers) ----------


fn tar_entries(path: &Path) -> Option<Vec<(String, u64, bool)>> {
    let f = File::open(path).ok()?;
    let zr = zstd::stream::read::Decoder::new(f).ok()?;
    let mut arch = tar::Archive::new(zr);
    let mut out = Vec::new();
    for entry in arch.entries().ok()? {
        let entry = entry.ok()?;
        let name = entry.path().ok()?.to_string_lossy().into_owned();
        // Skip the archive metadata `git archive` prepends (a real entry, not
        // content) and our own embedded sidecar.
        if name.is_empty()
            || name == "pax_global_header"
            || name.starts_with("pax_global_header/")
            || name.ends_with("/.reposilo.json")
        {
            continue;
        }
        let is_dir = entry.header().entry_type().is_dir();
        let size = entry.header().size().ok().unwrap_or(0);
        out.push((name, size, is_dir));
    }
    Some(out)
}

fn list_tar_zst(path: &Path) -> Option<Vec<ZipEntryView>> {
    let raw = tar_entries(path)?;
    let names: Vec<String> = raw.iter().map(|(n, _, _)| n.clone()).collect();
    let prefix = zip_common_prefix(&names);
    let strip = |n: &str| -> String {
        match &prefix {
            Some(p) => n.strip_prefix(&format!("{p}/")).unwrap_or(n).to_string(),
            None => n.to_string(),
        }
    };
    let mut out: Vec<ZipEntryView> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (name, size, _) in &raw {
        let stripped = strip(name);
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.split('/');
        let top = parts.next().unwrap_or("").to_string();
        let deeper = parts.next().is_some();
        if top.is_empty() {
            continue;
        }
        let key = top.to_lowercase();
        if seen.insert(key) {
            out.push(ZipEntryView { name: top, is_dir: deeper, size: if deeper { 0 } else { *size } });
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Some(out)
}

fn tar_find_entry(path: &Path, file: &str) -> Option<String> {
    let raw = tar_entries(path)?;
    let names: Vec<String> = raw.iter().map(|(n, _, _)| n.clone()).collect();
    let prefix = zip_common_prefix(&names);
    let full = match &prefix {
        Some(p) => format!("{p}/{file}"),
        None => file.to_string(),
    };
    if file != ".reposilo.json" && names.iter().any(|n| n == &full) {
        return Some(full);
    }
    None
}

fn tar_read_entry(path: &Path, entry: &str, max: usize) -> Option<Vec<u8>> {
    let f = File::open(path).ok()?;
    let zr = zstd::stream::read::Decoder::new(f).ok()?;
    let mut arch = tar::Archive::new(zr);
    for e in arch.entries().ok()? {
        let mut e = e.ok()?;
        if e.path().ok()?.to_string_lossy() == entry {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 16384];
            while buf.len() < max {
                let n = e.read(&mut chunk).ok()?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            return Some(buf);
        }
    }
    None
}

fn readme_from_tar_zst(path: &Path) -> Option<(String, String, bool)> {
    let raw = tar_entries(path)?;
    let names: Vec<String> = raw.iter().map(|(n, _, _)| n.clone()).collect();
    let prefix = zip_common_prefix(&names);
    let mk = |cand: &str| -> String {
        match &prefix {
            Some(p) => format!("{p}/{cand}"),
            None => cand.to_string(),
        }
    };
    for cand in ["README.md", "README.markdown", "README.txt", "README"] {
        let full = mk(cand);
        if raw.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(&full)) {
            let bytes = tar_read_entry(path, &full, 256 * 1024)?;
            let truncated = bytes.len() >= 256 * 1024;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            return Some((cand.to_string(), text, truncated));
        }
    }
    None
}

// ---------- zip-backed browsing (the zip is the only persistent artifact) ----------

use std::fs::File;
use std::io::Read as _;
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub struct ZipEntryView {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// The single top-level directory shared by every entry (e.g. the
/// `<project>/` prefix git archive writes), if there is one.
fn zip_common_prefix(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let candidate = first.split('/').next()?.to_string();
    if candidate.is_empty() || candidate.as_str() == first.as_str() {
        return None;
    }
    let shared = names.iter().all(|n| n == &candidate || n.starts_with(&format!("{candidate}/")));
    shared.then_some(candidate)
}

/// Root-level listing of a zip: (name after prefix-strip, is_dir, size).
/// Directories first, then files, case-insensitive by name.
pub fn list_zip(zip_path: &Path) -> Option<Vec<ZipEntryView>> {
    let names = zip_entry_names(zip_path)?;
    let prefix = zip_common_prefix(&names);
    let strip = |n: &str| -> String {
        match &prefix {
            Some(p) => n.strip_prefix(&format!("{p}/")).unwrap_or(n).to_string(),
            None => n.to_string(),
        }
    };
    let sizes: std::collections::HashMap<String, u64> = entry_sizes(zip_path);
    let mut out: Vec<ZipEntryView> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &names {
        let base = name.rsplit('/').next().unwrap_or(name);
        if name.is_empty() || base.eq_ignore_ascii_case(".reposilo.json") {
            continue;
        }
        let stripped = strip(name);
        if stripped.is_empty() {
            continue;
        }
        let mut parts = stripped.split('/');
        let top = parts.next().unwrap_or("").to_string();
        let deeper = parts.next().is_some();
        if top.is_empty() {
            continue;
        }
        let key = top.to_lowercase();
        if seen.insert(key) {
            out.push(ZipEntryView {
                name: top,
                is_dir: deeper,
                size: if deeper { 0 } else { *sizes.get(name).unwrap_or(&0) },
            });
        }
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then(a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    Some(out)
}

fn zip_entry_names(zip_path: &Path) -> Option<Vec<String>> {
    let f = File::open(zip_path).ok()?;
    let z = ZipArchive::new(f).ok()?;
    Some(z.file_names().map(String::from).collect())
}

fn entry_sizes(zip_path: &Path) -> std::collections::HashMap<String, u64> {
    let mut out = std::collections::HashMap::new();
    let Ok(f) = File::open(zip_path) else { return out };
    let Ok(mut z) = ZipArchive::new(f) else { return out };
    let names: Vec<String> = z.file_names().map(String::from).collect();
    for n in names {
        if let Ok(e) = z.by_name(&n) {
            out.insert(n, e.size());
        }
    }
    out
}

/// Locate the exact archive entry whose stripped name equals `file`.
/// Only names present in the central directory are reachable: this is
/// the traversal guard for the blob viewer.
pub fn zip_find_entry(zip_path: &Path, file: &str) -> Option<String> {
    let names = zip_entry_names(zip_path)?;
    let prefix = zip_common_prefix(&names);
    let full = match &prefix {
        Some(p) => format!("{p}/{file}"),
        None => file.to_string(),
    };
    if file != ".reposilo.json" && names.iter().any(|n| n == &full) {
        return Some(full);
    }
    None
}

/// Read one entry (exact archive name) from a zip, capped at `max` bytes.
pub fn zip_read(zip_path: &Path, entry: &str, max: usize) -> Option<Vec<u8>> {
    let f = File::open(zip_path).ok()?;
    let mut z = ZipArchive::new(f).ok()?;
    let mut e = z.by_name(entry).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    while buf.len() < max {
        let n = e.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(buf)
}

/// Root README text from a zip: (file name, raw text, truncated).
pub fn readme_from_zip(zip_path: &Path) -> Option<(String, String, bool)> {
    let names = zip_entry_names(zip_path)?;
    let prefix = zip_common_prefix(&names);
    let mk = |cand: &str| -> String {
        match &prefix {
            Some(p) => format!("{p}/{cand}"),
            None => cand.to_string(),
        }
    };
    for cand in ["README.md", "README.markdown", "README.txt", "README"] {
        let full = mk(cand);
        if names.iter().any(|n| n.eq_ignore_ascii_case(&full)) {
            let bytes = zip_read(zip_path, &full, 256 * 1024)?;
            let truncated = bytes.len() >= 256 * 1024;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            return Some((cand.to_string(), text, truncated));
        }
    }
    None
}

#[cfg(test)]
mod zip_tests {
    use super::*;

    fn make_zip(path: &Path, files: &[(&str, &str)]) {
        let f = File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in files {
            z.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut z, content.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn list_zip_strips_prefix_and_sorts_dirs_first() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.zip");
        make_zip(
            &p,
            &[
                ("proj/", ""),
                ("proj/README.md", "# Hi"),
                ("proj/src/", ""),
                ("proj/src/main.rs", "fn main(){}"),
                ("proj/docs.md", "d"),
                ("proj/.reposilo.json", "{}"),
            ],
        );
        let list = list_zip(&p).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "docs.md", "README.md"]); // dirs first, then files
        // is_dir flags
        let src = list.iter().find(|e| e.name == "src").unwrap();
        assert!(src.is_dir);
        let rd = list.iter().find(|e| e.name == "README.md").unwrap();
        assert!(!rd.is_dir);
        assert!(!names.contains(&".reposilo.json"), "metadata not listed");
    }

    #[test]
    fn find_and_read_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("b.zip");
        make_zip(&p, &[("proj/hello.txt", "line one\nline two\n")]);
        let entry = zip_find_entry(&p, "hello.txt").expect("entry found");
        assert_eq!(entry, "proj/hello.txt");
        let bytes = zip_read(&p, &entry, 1024).unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "line one\nline two\n");
        // traversal attempts find nothing
        assert!(zip_find_entry(&p, "../evil").is_none());
        assert!(zip_find_entry(&p, ".reposilo.json").is_none());
    }

    #[test]
    fn readme_from_zip_prefers_md() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("c.zip");
        make_zip(&p, &[("x/readme.txt", "txt readme"), ("x/README.md", "# MD\n\nbody")]);
        let (name, text, _) = readme_from_zip(&p).unwrap();
        assert_eq!(name, "README.md");
        assert!(text.contains("# MD"));
    }
}

#[cfg(test)]
mod tarzst_tests {
    use super::*;

    fn make_tar_zst(path: &Path, files: &[(&str, &str)]) {
        let f = File::create(path).unwrap();
        let enc = zstd::stream::write::Encoder::new(f, 3).unwrap();
        let mut b = tar::Builder::new(enc);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            b.append_data(&mut header, *name, content.as_bytes()).unwrap();
        }
        let enc = b.into_inner().unwrap();
        enc.finish().unwrap();
    }

    #[test]
    fn list_tar_zst_strips_prefix_and_hides_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("a.tar.zst");
        make_tar_zst(
            &p,
            &[
                ("proj/README.md", "# Hi"),
                ("proj/src/main.rs", "fn main(){}"),
                ("proj/docs.md", "d"),
                ("proj/.reposilo.json", "{}"),
            ],
        );
        let list = list_archive(&p).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "docs.md", "README.md"]);
        assert!(!names.contains(&".reposilo.json"), "metadata must be hidden");
        assert!(list.iter().find(|e| e.name == "src").unwrap().is_dir);
    }

    /// `git archive --format=tar` prepends a `pax_global_header` entry. It must
    /// not defeat the common-prefix stripping or show up as a file.
    #[test]
    fn list_tar_zst_ignores_pax_global_header() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pax.tar.zst");
        make_tar_zst(
            &p,
            &[
                ("pax_global_header", "52 comment=abc"),
                ("proj/README.md", "# Hi"),
                ("proj/src/main.rs", "fn main(){}"),
            ],
        );
        let list = list_archive(&p).unwrap();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["src", "README.md"], "pax header must be skipped");
        let readme = readme_from_archive(&p).expect("readme found despite pax header");
        assert_eq!(readme.0, "README.md");
    }

    #[test]
    fn find_and_read_tar_zst_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("b.tar.zst");
        make_tar_zst(&p, &[("proj/hello.txt", "line one\nline two\n")]);
        let entry = find_archive_entry(&p, "hello.txt").expect("entry found");
        assert_eq!(entry, "proj/hello.txt");
        let bytes = read_archive(&p, &entry, 1024).unwrap();
        assert_eq!(String::from_utf8_lossy(&bytes), "line one\nline two\n");
        // traversal / metadata guarded the same way as zips
        assert!(find_archive_entry(&p, "../evil").is_none());
        assert!(find_archive_entry(&p, ".reposilo.json").is_none());
    }

    #[test]
    fn readme_from_tar_zst_prefers_md() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("c.tar.zst");
        make_tar_zst(&p, &[("x/readme.txt", "txt"), ("x/README.md", "# MD\n\nbody")]);
        let (name, text, _) = readme_from_archive(&p).unwrap();
        assert_eq!(name, "README.md");
        assert!(text.contains("# MD"));
    }
}
