//! Import a directory of zipped repos into the archive
//!
//! Offline detection first (.git/config, go.mod, package.json, Cargo.toml,
//! pyproject.toml, README), GitHub search suggestions + local LLM for the
//! rest, and unidentified zips park in `_unknown/` as first-class, taggable
//! archive entries awaiting a manual origin assignment.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use zip::ZipArchive;

use crate::config::Config;
use crate::files::detect_language;
use crate::types::{RepoManifest, SnapshotSidecar, ZipInfo, write_json};

// ---------- scan ----------

/// One zip found during a scan, with everything we could detect offline.
#[derive(Debug, Clone, Serialize)]
pub struct ScanRow {
    pub source: String,
    pub file_name: String,
    pub size: u64,
    pub detected_origin: Option<String>,
    pub detected_name: Option<String>,
    pub detected_description: Option<String>,
    /// First ~400 chars of the README (for the UI + LLM identification).
    pub readme_excerpt: String,
    /// Top-level file/dir names, for the UI + LLM identification.
    pub file_sample: Vec<String>,
    /// Primary language detected from the zip's entries (count-weighted).
    pub detected_language: Option<String>,
    /// Which detection sources produced evidence, e.g. [".git/config", "Cargo.toml"].
    pub evidence: Vec<String>,
}

/// A stored scan (server-side state keyed by id so the review form can
/// reference rows without re-posting zip paths).
#[derive(Debug, Clone, Serialize)]
pub struct ImportScan {
    pub rows: Vec<ScanRow>,
    pub created: String,
}

/// User's decision for one row at commit time.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ImportRow {
    pub i: usize,
    pub origin: Option<String>,
    pub unknown: bool,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub file: String,
    pub repo: String,
    pub action: String, // "imported" | "unknown-parked" | "skipped" | "failed"
    pub detail: String,
}

// ---------- zip reading helpers ----------

fn base_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

fn read_entry(z: &mut ZipArchive<fs::File>, name: &str, max: usize) -> Option<String> {
    let mut e = z.by_name(name).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    while buf.len() < max {
        let n = e.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    // reject binary
    if buf.iter().take(4096).any(|&b| b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn open_zip(path: &Path) -> Result<ZipArchive<fs::File>> {
    let f = fs::File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
    ZipArchive::new(f).with_context(|| format!("cannot read zip {}", path.display()))
}

// ---------- detection ----------

/// Normalize a candidate origin: strip git+/ssh prefixes, .git suffix, and
/// expand npm-style shorthand ("github:user/repo") to an https URL.
fn normalize_origin(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() < 5 || s.contains(char::is_whitespace) {
        return None;
    }
    let s = s.strip_prefix("git+").unwrap_or(s);
    let s = s.strip_suffix(".git").unwrap_or(s);
    if let Some(rest) = s.strip_prefix("github:") {
        if rest.split('/').count() == 2 && !rest.contains("..") {
            return Some(format!("https://github.com/{rest}"));
        }
        return None;
    }
    if s.starts_with("https://") || s.starts_with("http://") || s.starts_with("git@") {
        return Some(s.to_string());
    }
    if s.starts_with("ssh://") {
        return Some(s.to_string());
    }
    None
}

/// Pull `url = ...` from the `[remote "origin"]` section of a git config.
fn git_config_origin(text: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_section = t == "[remote \"origin\"]";
            continue;
        }
        if in_section {
            if let Some((k, v)) = t.split_once('=') {
                if k.trim() == "url" {
                    return normalize_origin(v.trim());
                }
            }
        }
    }
    None
}

/// Extract (name, origin) from package.json.
fn package_json(text: &str) -> (Option<String>, Option<String>) {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let name = v["name"].as_str().map(|s| s.split('/').next_back().unwrap_or(s).to_string());
    let origin = match &v["repository"] {
        serde_json::Value::String(s) => normalize_origin(s),
        serde_json::Value::Object(o) => o["url"].as_str().and_then(normalize_origin),
        _ => None,
    };
    (name, origin)
}

/// Extract (name, origin) from Cargo.toml / pyproject.toml via the toml crate.
fn toml_manifest(text: &str) -> (Option<String>, Option<String>) {
    let v: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let name = v
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .or_else(|| v.get("project").and_then(|p| p.get("name")).and_then(|n| n.as_str()))
        .map(str::to_string);
    let origin = v
        .get("package")
        .and_then(|p| p.get("repository"))
        .and_then(|r| r.as_str())
        .and_then(normalize_origin)
        .or_else(|| {
            v.get("project")
                .and_then(|p| p.get("urls"))
                .and_then(|u| u.get("Repository").or_else(|| u.get("repository")))
                .and_then(|r| r.as_str())
                .and_then(normalize_origin)
        });
    (name, origin)
}

/// `module github.com/foo/bar` is literally the origin.
fn go_mod_origin(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(m) = t.strip_prefix("module ") {
            let m = m.trim();
            if m.starts_with("github.com/") && m.split('/').count() >= 3 {
                return Some(format!("https://{m}"));
            }
        }
    }
    None
}

fn readme_name_and_desc(text: &str) -> (Option<String>, Option<String>) {
    let name = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("# ").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty());
    let desc = crate::files::first_paragraph(text);
    (name, desc)
}

const README_NAMES: &[&str] = &["readme.md", "readme.markdown", "readme.txt", "readme"];

/// Scan one zip and detect what we can, fully offline.
pub fn scan_zip(path: &Path) -> ScanRow {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown.zip".into());
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let mut row = ScanRow {
        source: path.to_string_lossy().into_owned(),
        file_name: file_name.clone(),
        size,
        detected_origin: None,
        detected_name: None,
        detected_description: None,
        readme_excerpt: String::new(),
        file_sample: Vec::new(),
        detected_language: None,
        evidence: Vec::new(),
    };

    let Ok(mut z) = open_zip(path) else {
        return row; // unreadable zip; still listed so the user can deal with it
    };
    let names: Vec<String> = z.file_names().map(String::from).collect();
    if names.is_empty() {
        return row;
    }

    // top-level entries (strip the common leading dir, e.g. "repo-main/")
    let mut tops: Vec<String> = Vec::new();
    for n in &names {
        let mut parts = n.split('/');
        let first = parts.next().unwrap_or("");
        let second = parts.next().unwrap_or("");
        let label = if !second.is_empty() && !first.is_empty() { second } else { first };
        if !label.is_empty() && !tops.iter().any(|t| t == label) {
            tops.push(label.to_string());
        }
        if tops.len() >= 15 {
            break;
        }
    }
    row.file_sample = tops;

    // language from all entry names (count-weighted; no decompression)
    row.detected_language = detect_language(
        &names.iter().map(|n| (n.clone(), 1u64)).collect::<Vec<_>>(),
    );

    // 1. .git/config → origin
    let mut read_name: Option<String> = None;
    if let Some(cfg_name) = names.iter().find(|n| n.ends_with(".git/config")).cloned() {
        if let Some(text) = read_entry(&mut z, &cfg_name, 8 * 1024) {
            if let Some(origin) = git_config_origin(&text) {
                row.detected_origin = Some(origin);
                row.evidence.push(".git/config".into());
            }
        }
    }
    // 2. go.mod
    if row.detected_origin.is_none() {
        if let Some(n) = names.iter().find(|n| base_name(n).eq_ignore_ascii_case("go.mod")).cloned() {
            if let Some(text) = read_entry(&mut z, &n, 8 * 1024) {
                if let Some(origin) = go_mod_origin(&text) {
                    row.detected_origin = Some(origin);
                    row.evidence.push("go.mod".into());
                }
            }
        }
    }
    // 3. package.json / Cargo.toml / pyproject.toml (match on basename at
    //    the shallowest depth, so root-level manifests are found too)
    if row.detected_origin.is_none() {
        if let Some(n) = shallowest_match(&names, "package.json") {
            if let Some(text) = read_entry(&mut z, &n, 64 * 1024) {
                let (name, origin) = package_json(&text);
                read_name = name.or(read_name);
                if let Some(origin) = origin {
                    row.detected_origin = Some(origin);
                    row.evidence.push("package.json".into());
                }
            }
        }
    }
    if row.detected_origin.is_none() {
        for manifest in ["Cargo.toml", "pyproject.toml"] {
            if let Some(n) = shallowest_match(&names, manifest) {
                if let Some(text) = read_entry(&mut z, &n, 32 * 1024) {
                    let (name, origin) = toml_manifest(&text);
                    read_name = name.or(read_name);
                    if let Some(origin) = origin {
                        row.detected_origin = Some(origin);
                        row.evidence.push(manifest.into());
                    }
                }
                if row.detected_origin.is_some() {
                    break;
                }
            }
        }
    }
    if read_name.is_some() && row.detected_name.is_none() {
        row.detected_name = read_name.clone();
    }
    // 4. README → name + description + excerpt (README title wins over
    //    manifest names: it is the human-readable project name)
    if let Some(n) = names
        .iter()
        .filter(|n| README_NAMES.iter().any(|r| n.ends_with(r) || base_name(n).eq_ignore_ascii_case(r)))
        .min_by_key(|n| n.matches('/').count())
        .cloned()
    {
        if let Some(text) = read_entry(&mut z, &n, 64 * 1024) {
            let (name, desc) = readme_name_and_desc(&text);
            if name.is_some() {
                row.detected_name = name.or(row.detected_name.take());
            }
            row.detected_description = desc;
            row.readme_excerpt = text.chars().take(400).collect();
            row.evidence.push("README".into());
        }
    } else {
        // no README: prefer a shared non-generic top-level dir (git archive
        // exports prefix entries with the repo name), then the file stem
        row.detected_name = row
            .detected_name
            .take()
            .or_else(|| common_prefix(&names))
            .or_else(|| Some(file_name.trim_end_matches(".zip").to_string()));
    }
    row
}

/// Entry whose basename matches, preferring the shallowest path depth.
fn shallowest_match(names: &[String], base: &str) -> Option<String> {
    names
        .iter()
        .filter(|n| base_name(n).eq_ignore_ascii_case(base))
        .min_by_key(|n| n.matches('/').count())
        .cloned()
}

/// The single top-level directory shared by every entry, if it looks like a
/// project name rather than a generic folder ("src", "data", ...).
fn common_prefix(names: &[String]) -> Option<String> {
    let first = names.first()?;
    let candidate = first.split('/').next()?.to_string();
    if candidate.is_empty() || candidate.as_str() == first.as_str() {
        return None; // no directory level at all
    }
    let shared = names
        .iter()
        .all(|n| n == &candidate || n.starts_with(&format!("{candidate}/")));
    if !shared {
        return None;
    }
    const GENERIC: &[&str] = &[
        "src", "data", "dist", "build", "out", "assets", "repo", "main", "master", "app",
        "project", "code", "source", "release", "bin",
    ];
    if GENERIC.contains(&candidate.to_lowercase().as_str()) {
        return None;
    }
    Some(candidate)
}

/// Scan a directory tree for zips.
pub fn scan_dir(dir: &Path) -> Result<ImportScan> {
    let mut rows = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let entries = fs::read_dir(&d).with_context(|| format!("cannot read dir {}", d.display()))?;
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "shallow.git" || name.starts_with('.') {
                    continue;
                }
                stack.push(p);
            } else if p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("zip")).unwrap_or(false) {
                rows.push(scan_zip(&p));
            }
        }
    }
    rows.sort_by(|a, b| a.file_name.cmp(&b.file_name));
    Ok(ImportScan { rows, created: crate::archiver::now_rfc3339() })
}

// ---------- import ----------

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    // cross-filesystem: copy + delete
    fs::copy(from, to).with_context(|| format!("cannot copy {} -> {}", from.display(), to.display()))?;
    fs::remove_file(from).with_context(|| format!("cannot remove original {}", from.display()))?;
    Ok(())
}

fn unique_path(mut p: PathBuf) -> PathBuf {
    if !p.exists() {
        return p;
    }
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_os_string()).unwrap_or_default();
    let mut n = 1;
    while p.exists() {
        p = p.with_file_name(format!(
            "{}_{n}.{}",
            stem.to_string_lossy(),
            ext.to_string_lossy()
        ));
        n += 1;
    }
    p
}

fn sanitize(s: &str) -> String {
    crate::archiver::sanitize(s)
}

/// Import one scanned row. `origin` is the user-confirmed origin (or the
/// detected one); `unknown` parks it in `_unknown/`.
#[allow(clippy::too_many_arguments)]
pub async fn import_one(
    cfg: &Config,
    row: &ScanRow,
    origin: Option<&str>,
    tags: &[String],
    unknown: bool,
    keep_original: bool,
) -> ImportOutcome {
    let src = PathBuf::from(&row.source);
    let fail = |detail: String| ImportOutcome {
        file: row.file_name.clone(),
        repo: String::new(),
        action: "failed".into(),
        detail,
    };
    let move_failed = |e: std::io::Error| fail(format!("cannot read zip: {e}"));

    let root = PathBuf::from(&cfg.archive.root);
    let stem = row
        .file_name
        .strip_suffix(".zip")
        .or_else(|| row.file_name.strip_suffix(".ZIP"))
        .unwrap_or(&row.file_name);

    // resolve repo dir
    let mut manifest_existing: Option<RepoManifest> = None;
    let (repo_dir, rel) = if unknown {
        let dir = root.join("_unknown").join(sanitize(stem));
        (dir, format!("_unknown/{}", sanitize(stem)))
    } else {
        let origin = origin
            .map(str::to_string)
            .or_else(|| row.detected_origin.clone())
            .unwrap_or_default();
        if origin.is_empty() {
            return fail("no origin provided and none detected".into());
        }
        let info = crate::forge::detect(&origin).ok();
        let owner = info
            .as_ref()
            .map(|i| sanitize(&i.owner))
            .unwrap_or_else(|| "imported".into());
        let name = info
            .as_ref()
            .map(|i| sanitize(&i.name))
            .unwrap_or_else(|| sanitize(stem));
        let slug = format!("{owner}-{name}");
        let dir = root.join(&slug);
        let rel = slug;
        if dir.join("repo.json").exists() {
            manifest_existing = crate::types::read_json::<RepoManifest>(&dir.join("repo.json")).ok();
        }
        (dir, rel)
    };

    // verify the source zip is readable before mutating anything
    if open_zip(&src).is_err() {
        if !src.exists() {
            return fail("source zip vanished".into());
        }
        return fail("source zip unreadable".into());
    }
    if fs::metadata(&src).map(|m| m.len()).unwrap_or(0) != row.size {
        return fail("source zip changed since scan; re-scan".into());
    }

    let action = if unknown { "unknown-parked" } else { "imported" };

    // manifest: create new or extend existing
    let manifest = if let Some(mut m) = manifest_existing.take() {
        for t in tags {
            if !m.tags.iter().any(|e| e.eq_ignore_ascii_case(t)) {
                m.tags.push(t.clone());
            }
        }
        m
    } else {
        let forge = if unknown {
            "unknown".to_string()
        } else {
            crate::forge::detect(
                row.detected_origin
                    .as_deref()
                    .or(origin)
                    .unwrap_or_default(),
            )
            .map(|i| i.kind.id().to_string())
            .unwrap_or_else(|_| "generic".into())
        };
        RepoManifest {
            origin: if unknown { None } else { Some(origin.map(str::to_string).or_else(|| row.detected_origin.clone()).unwrap_or_default()) },
            forge,
            name: row.detected_name.clone().unwrap_or_else(|| stem.to_string()),
            added: crate::archiver::now_rfc3339(),
            tags: if unknown && tags.is_empty() {
                vec!["unidentified".into()]
            } else {
                tags.to_vec()
            },
            description: row.detected_description.clone(),
            language: row.detected_language.clone(),
            default_branch: "unknown".into(),
            schedule: Default::default(),
            retention: Default::default(),
            last_checked: None,
            notes: None,
            remote_state: None,
            unavailable_since: None,
            suggested_tags: Vec::new(),
            stars: None,
            unidentified: unknown,
        }
    };
    let _ = move_failed; // (moved earlier via is_err checks)

    // place the zip under branch/imported/ (never touched by retention pruning)
    let snap_dir = repo_dir.join("branch").join("imported");
    if fs::create_dir_all(&snap_dir).is_err() {
        return fail(format!("cannot create {}", snap_dir.display()));
    }
    let target = unique_path(snap_dir.join(format!("{}.zip", sanitize(stem))));
    if keep_original {
        if fs::copy(&src, &target).is_err() {
            return fail(format!("cannot copy to {}", target.display()));
        }
    } else if move_file(&src, &target).is_err() {
        return fail(format!("cannot move to {}", target.display()));
    }

    // sidecar
    let zip_meta = fs::metadata(&target).ok();
    let (bytes, sha256) = match crate::archiver::sha256_file(&target) {
        Ok(v) => v,
        Err(e) => return fail(format!("hash failed: {e}")),
    };
    let sidecar = SnapshotSidecar {
        kind: "imported".into(),
        repo: rel.clone(),
        origin: manifest.origin.clone().unwrap_or_default(),
        r#ref: sanitize(stem),
        version: None,
        commit: "unknown".into(),
        committed_at: None,
        archived_at: crate::archiver::now_rfc3339(),
        archiver_version: env!("CARGO_PKG_VERSION").into(),
        imported: Some(true),
        imported_from: Some(row.source.clone()),
        format: Some("zip".into()),
        changelog: None,
        assets: Vec::new(),
        assets_filters: Vec::new(),
        assets_max_mb: 0,
        zip: ZipInfo {
            file: target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            bytes: bytes.max(zip_meta.map(|m| m.len()).unwrap_or(0)).min(bytes),
            sha256,
        },
    };
    let json_path = target.with_extension("json");
    if write_json(&json_path, &sidecar).is_err() {
        return fail("cannot write sidecar".into());
    }
    if write_json(&repo_dir.join("repo.json"), &manifest).is_err() {
        return fail("cannot write manifest".into());
    }
    // plain README for browsing, extracted from the imported zip
    if let Some((_, text, _)) = crate::files::readme_from_zip(&target) {
        let _ = fs::write(repo_dir.join("README.md"), text);
    }

    let _ = row.size;
    ImportOutcome {
        file: row.file_name.clone(),
        repo: rel,
        action: action.into(),
        detail: format!(
            "{} {} ({} bytes)",
            if keep_original { "copied" } else { "moved" },
            target.display(),
            bytes
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_zip(path: &Path, files: &[(&str, &str)]) {
        let f = fs::File::create(path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in files {
            z.start_file(*name, opts).unwrap();
            std::io::Write::write_all(&mut z, content.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }

    #[test]
    fn detects_git_config_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("mystery.zip");
        make_zip(
            &p,
            &[
                ("proj/.git/config", "[core]\n\trepositoryformatversion = 0\n[remote \"origin\"]\n\turl = https://github.com/foo/bar.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n"),
                ("proj/README.md", "# Bar\n\nA test project.\n"),
            ],
        );
        let row = scan_zip(&p);
        assert_eq!(row.detected_origin.as_deref(), Some("https://github.com/foo/bar"));
        assert_eq!(row.detected_name.as_deref(), Some("Bar"));
        assert!(row.evidence.contains(&".git/config".to_string()));
    }

    #[test]
    fn detects_package_json_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pkg.zip");
        make_zip(
            &p,
            &[("package.json", "{\"name\": \"left-pad\", \"repository\": \"github:saorisa/left-pad\"}")],
        );
        let row = scan_zip(&p);
        assert_eq!(row.detected_origin.as_deref(), Some("https://github.com/saorisa/left-pad"));
        assert_eq!(row.detected_name.as_deref(), Some("left-pad"));
    }

    #[test]
    fn detects_cargo_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("c.zip");
        make_zip(
            &p,
            &[("Cargo.toml", "[package]\nname = \"ripgrep\"\nrepository = \"https://github.com/BurntSushi/ripgrep\"\nversion = \"1.0\"\n")],
        );
        let row = scan_zip(&p);
        assert_eq!(row.detected_origin.as_deref(), Some("https://github.com/BurntSushi/ripgrep"));
        assert_eq!(row.detected_name.as_deref(), Some("ripgrep"));
    }

    #[test]
    fn detects_go_mod_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("go.zip");
        make_zip(&p, &[("go.mod", "module github.com/spf13/hugo\n\ngo 1.21\n")]);
        let row = scan_zip(&p);
        assert_eq!(row.detected_origin.as_deref(), Some("https://github.com/spf13/hugo"));
    }

    #[test]
    fn unidentifiable_zip_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("random.zip");
        make_zip(&p, &[("data/notes.txt", "just some notes\n")]);
        let row = scan_zip(&p);
        assert!(row.detected_origin.is_none());
        // falls back to file stem as name
        assert_eq!(row.detected_name.as_deref(), Some("random"));
    }

    #[test]
    fn detects_language_from_zip_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("pys.zip");
        make_zip(
            &p,
            &[
                ("proj/README.md", "# PyProj\n"),
                ("proj/app.py", "print(1)\n"),
                ("proj/lib.py", "x = 2\n"),
                ("proj/notes.md", "docs\n"),
            ],
        );
        let row = scan_zip(&p);
        assert_eq!(row.detected_language.as_deref(), Some("Python"));
    }

    #[test]
    fn normalize_origin_variants() {
        assert_eq!(
            normalize_origin("git+https://github.com/a/b.git").as_deref(),
            Some("https://github.com/a/b")
        );
        assert_eq!(normalize_origin("ssh://git@gitlab.com/x/y.git").as_deref(), Some("ssh://git@gitlab.com/x/y"));
        assert!(normalize_origin("not a url at all").is_none());
        assert!(normalize_origin("").is_none());
    }
}