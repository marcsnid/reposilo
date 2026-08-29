//! Import flow tests: scan/detect zips, commit via the web form endpoints,
//! park unknowns, and verify archive + index state on disk.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;

use reposilo::config::Config;
use reposilo::importer::{scan_dir, scan_zip, import_one};
use reposilo::server::{router, AppState};

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

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn test_cfg(root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.archive.root = root.to_string_lossy().into_owned();
    cfg
}

async fn spawn_server(cfg: Config) -> String {
    let st = Arc::new(AppState::new(cfg, None).await.expect("state init"));
    let app = router(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[test]
fn scan_dir_finds_and_detects() {
    let tmp = tempfile::tempdir().unwrap();
    let collection = tmp.path().join("collection");
    fs::create_dir_all(&collection).unwrap();
    make_zip(
        &collection.join("cloned-repo.zip"),
        &[
            ("proj/.git/config", "[remote \"origin\"]\n\turl = https://github.com/user/cloned-repo.git\n"),
            ("proj/README.md", "# Cloned Repo\n\nIt was a zipped clone.\n"),
        ],
    );
    make_zip(
        &collection.join("cargo-project.zip"),
        &[("Cargo.toml", "[package]\nname = \"mycrate\"\nversion = \"0.1\"\nrepository = \"https://github.com/someone/mycrate\"\n")],
    );
    make_zip(
        &collection.join("mystery.zip"),
        &[("stuff/picture.bin", "\u{0}\u{0}\u{1}binary")],
    );
    fs::write(collection.join("notazip.txt"), "hi").unwrap();

    let scan = scan_dir(&collection).unwrap();
    assert_eq!(scan.rows.len(), 3, "3 zips (txt ignored)");
    let by_name = |n: &str| scan.rows.iter().find(|r| r.file_name == n).unwrap();
    assert_eq!(
        by_name("cloned-repo.zip").detected_origin.as_deref(),
        Some("https://github.com/user/cloned-repo")
    );
    assert_eq!(
        by_name("cargo-project.zip").detected_origin.as_deref(),
        Some("https://github.com/someone/mycrate")
    );
    assert!(by_name("mystery.zip").detected_origin.is_none());
}

#[tokio::test]
async fn import_one_known_and_unknown() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    let collection = tmp.path().join("collection");
    fs::create_dir_all(&collection).unwrap();
    make_zip(
        &collection.join("known.zip"),
        &[("proj/.git/config", "[remote \"origin\"]\n\turl = https://github.com/foo/known.git\n")],
    );
    make_zip(&collection.join("unknown.zip"), &[("a/b.txt", "mystery")]);
    let archive = tmp.path().join("archive");

    let cfg = test_cfg(&archive);
    let row = scan_zip(&collection.join("known.zip"));
    let out = import_one(&cfg, &row, None, &["imported-tag".to_string()], false, false).await;
    assert_eq!(out.action, "imported", "{}", out.detail);
    assert_eq!(out.repo, "foo-known");
    // zip moved, manifest + sidecar written
    assert!(!collection.join("known.zip").exists());
    let repo = archive.join("foo-known");
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("repo.json"))?)?;
    assert_eq!(m["origin"], "https://github.com/foo/known");
    assert_eq!(m["tags"][0], "imported-tag");
    assert_eq!(m["default_branch"], "unknown");
    let snap = repo.join("branch/imported/known.zip");
    assert!(snap.exists());
    let sc: serde_json::Value = serde_json::from_str(&fs::read_to_string(snap.with_extension("json"))?)?;
    assert_eq!(sc["kind"], "imported");
    assert_eq!(sc["imported"], true);
    assert_eq!(sc["imported_from"], row.source);
    assert_eq!(sc["repo"], "foo-known");

    // unknown parking
    let row = scan_zip(&collection.join("unknown.zip"));
    let out = import_one(&cfg, &row, None, &[], true, false).await;
    assert_eq!(out.action, "unknown-parked");
    assert_eq!(out.repo, "_unknown/unknown");
    let repo = archive.join("_unknown/unknown");
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("repo.json"))?)?;
    assert!(m["origin"].is_null());
    assert_eq!(m["unidentified"], true);
    assert_eq!(m["tags"][0], "unidentified");
    assert!(repo.join("branch/imported/unknown.zip").exists());

    // index sees both; tag filter works
    let index = reposilo::index::Index::load(&archive)?;
    assert_eq!(index.repos.len(), 2);
    assert_eq!(index.filter(&["imported-tag".into()], None).len(), 1);
    assert_eq!(index.filter(&["unidentified".into()], None).len(), 1);

    // keep_original copies instead of moving
    make_zip(&collection.join("keepme.zip"), &[("x.txt", "data")]);
    let row = scan_zip(&collection.join("keepme.zip"));
    let out = import_one(&cfg, &row, None, &["k".to_string()], true, true).await;
    assert_eq!(out.action, "unknown-parked");
    assert!(collection.join("keepme.zip").exists(), "original must survive with keep_original");
    Ok(())
}

#[tokio::test]
async fn web_import_flow_scan_and_commit() -> Result<()> {
    let tmp = tempfile::tempdir().unwrap();
    let collection = tmp.path().join("collection");
    fs::create_dir_all(&collection).unwrap();
    make_zip(
        &collection.join("hello.zip"),
        &[
            ("proj/.git/config", "[remote \"origin\"]\n\turl = https://github.com/x/hello.git\n"),
            ("proj/README.md", "# Hello\n\nA hello world project.\n"),
        ],
    );
    make_zip(&collection.join("mystery.zip"), &[("d/f.txt", "x")]);

    let base = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // import page exists
    let page = client.get(format!("{base}/import")).send().await?.text().await?;
    assert!(page.contains("Import a zip collection"));

    // scan via the form (like htmx sends)
    let rows = client
        .post(format!("{base}/import/scan"))
        .form(&[("dir", collection.to_string_lossy().as_ref())])
        .send()
        .await?
        .text()
        .await?;
    assert!(rows.contains("hello.zip"), "{rows}");
    assert!(rows.contains("mystery.zip"));
    assert!(rows.contains("https://github.com/x/hello"), "detected origin prefilled");
    assert!(rows.contains("include_0"), "indexed checkboxes");
    let scan_id: String = rows
        .split("name=\"scan\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next().map(String::from))
        .expect("scan id in form");

    // commit both rows: row 0 as detected, row 1 as unknown
    let resp = client
        .post(format!("{base}/import/commit"))
        .form(&[
            ("scan", scan_id.as_str()),
            ("include_0", "1"),
            ("origin_0", "https://github.com/x/hello.git"),
            ("tags_0", "web-import"),
            ("include_1", "1"),
            ("unknown_1", "1"),
        ])
        .send()
        .await?
        .text()
        .await?;
    assert!(resp.contains("Importing"), "{resp}");

    // wait for the job, then poll its status
    let job_id: u64 = resp
        .split("/repos/job-status/")
        .nth(1)
        .and_then(|s| s.split(['"', '?', '<']).next())
        .and_then(|s| s.parse().ok())
        .expect("job id");
    let mut note = String::new();
    for _ in 0..100 {
        let st: String = client
            .get(format!("{base}/repos/job-status/{job_id}"))
            .send()
            .await?
            .text()
            .await?;
        if !st.contains("hx-trigger=\"every") {
            note = st;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(note.contains("Imported 1, parked 1 unknown, 0 failed"), "{note}");
    assert!(note.contains("hx-swap-oob"), "grid refresh on completion: {note}");

    // archive state on disk
    let archive = tmp.path().join("archive");
    assert!(archive.join("x-hello/branch/imported/hello.zip").exists());
    assert!(archive.join("_unknown/mystery/branch/imported/mystery.zip").exists());

    assert!(!collection.join("hello.zip").exists(), "moved");
    assert!(!collection.join("mystery.zip").exists(), "moved");

    // plain README extracted from the imported zip next to the metadata
    assert!(
        fs::read_to_string(archive.join("x-hello/README.md"))
            .unwrap_or_default()
            .contains("A hello world project")
    );
    // UI shows them: filtered by the tag we applied, plus the unknown pile
    let page = client.get(format!("{base}/?tags=web-import")).send().await?.text().await?;
    assert!(page.contains("hello"), "imported repo card should render: {page}");
    let page = client.get(format!("{base}/?tags=unidentified")).send().await?.text().await?;
    assert!(page.contains("mystery"), "unknown pile should render: {page}");
    Ok(())
}
#[tokio::test]
async fn assigning_origin_promotes_unknown_and_self_heals() -> Result<()> {
    // a real local remote the mystery zip "came from"
    let tmp = tempfile::tempdir()?;
    let (remote, _work) = make_remote_for_origin_test(tmp.path());

    let archive = tmp.path().join("archive");
    let collection = tmp.path().join("collection");
    fs::create_dir_all(&collection).unwrap();
    make_zip(&collection.join("mystery.zip"), &[("stuff/notes.txt", "mystery")]);

    let base = spawn_server(test_cfg(&archive)).await;
    let client = reqwest::Client::new();

    // import as unknown
    let rows = client
        .post(format!("{base}/import/scan"))
        .form(&[("dir", collection.to_string_lossy().as_ref())])
        .send()
        .await?
        .text()
        .await?;
    let scan_id: String = rows
        .split("name=\"scan\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next().map(String::from))
        .expect("scan id");
    client
        .post(format!("{base}/import/commit"))
        .form(&[("scan", scan_id.as_str()), ("include_0", "1"), ("unknown_0", "1")])
        .send()
        .await?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if archive.join("_unknown/mystery/repo.json").exists() {
            break;
        }
    }
    assert!(archive.join("_unknown/mystery/repo.json").exists());

    // assign the origin via the API (the promotion flow)
    let patched: serde_json::Value = client
        .patch(format!("{base}/api/repos/_unknown/mystery"))
        .json(&serde_json::json!({ "origin": file_url(&remote) }))
        .send()
        .await?
        .json()
        .await?;
    assert!(patched["unidentified"] == false || patched.get("unidentified").is_none());
    let new_rel = patched["moved_to"].as_str().expect("moved").to_string();
    assert!(!new_rel.contains("_unknown"), "promoted out of _unknown: {new_rel}");
    // old location is gone, new flat folder exists
    assert!(!archive.join("_unknown/mystery").exists());
    assert!(archive.join(&new_rel).join("repo.json").exists());
    // the zip came along
    assert!(archive.join(&new_rel).join("branch/imported/mystery.zip").exists());

    // self-heal: refresh clones the origin, fixes default branch, snapshots
    let dir = archive.join(&new_rel);
    let archiver = reposilo::archiver::Archiver::new(test_cfg(&archive));
    let summary = archiver.refresh_repo(&dir).await?;
    assert!(!summary.remote_unavailable, "local remote should be reachable");
    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(dir.join("repo.json"))?)?;
    assert_eq!(m["default_branch"], "master", "default branch healed: {}", m["default_branch"]);
    // a fresh branch snapshot exists alongside the imported zip
    assert!(dir.join("branch/master").is_dir());

    // README was extracted from the new snapshot (zip-only flow)
    assert!(fs::read_to_string(dir.join("README.md")).unwrap_or_default().contains("hello"));
    Ok(())
}

fn make_remote_for_origin_test(parent: &Path) -> (PathBuf, PathBuf) {
    use std::process::Command;
    let run = |args: &[&str], cwd: Option<&Path>| {
        let mut c = Command::new("git");
        c.args(args);
        if let Some(d) = cwd {
            c.current_dir(d);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let remotes = parent.join("remotes");
    fs::create_dir_all(&remotes).unwrap();
    let remote = remotes.join("originrepo.git");
    run(&["init", "--bare", "-b", "master", remote.to_str().unwrap()], None);
    let work = parent.join("work-origin");
    run(&["init", "-b", "master", work.to_str().unwrap()], None);
    fs::write(work.join("README.md"), "# hello\n").unwrap();
    run(&["add", "."], Some(&work));
    run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "i"], Some(&work));
    run(&["remote", "add", "origin", remote.to_str().unwrap()], Some(&work));
    run(&["push", "origin", "master"], Some(&work));
    (remote, work)
}

#[tokio::test]
async fn metadata_editor_updates_manifest() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive).unwrap();
    let collection = tmp.path().join("collection");
    fs::create_dir_all(&collection).unwrap();
    make_zip(&collection.join("mystery.zip"), &[("stuff/readme.txt", "mystery")]);

    let base = spawn_server(test_cfg(&archive)).await;
    let client = reqwest::Client::new();

    // import as unknown
    let rows = client
        .post(format!("{base}/import/scan"))
        .form(&[("dir", collection.to_string_lossy().as_ref())])
        .send()
        .await?
        .text()
        .await?;
    let scan_id: String = rows
        .split("name=\"scan\" value=\"")
        .nth(1)
        .and_then(|s| s.split('"').next().map(String::from))
        .expect("scan id");
    client
        .post(format!("{base}/import/commit"))
        .form(&[("scan", scan_id.as_str()), ("include_0", "1"), ("unknown_0", "1")])
        .send()
        .await?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if archive.join("_unknown/mystery/repo.json").exists() {
            break;
        }
    }

    // Jellyfin-style: edit the metadata (incl. origin) via the form
    let resp = client
        .post(format!("{base}/repos/_unknown/mystery/metadata"))
        .form(&[
            ("name", "Mystery Solved"),
            ("description", "it was a mystery but now it's not"),
            ("notes", "private notes here"),
            ("origin", "https://github.com/example/solved.git"),
        ])
        .send()
        .await?
        .text()
        .await?;
    assert!(resp.contains("metadata saved"), "{resp}");
    assert!(resp.contains("example-solved"), "repo should have been promoted: {resp}");

    // manifest reflects everything, repo moved out of _unknown
    assert!(!archive.join("_unknown/mystery").exists(), "old location gone");
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(archive.join("example-solved/repo.json"))?)?;
    assert_eq!(m["name"], "Mystery Solved");
    assert_eq!(m["description"], "it was a mystery but now it's not");
    assert_eq!(m["notes"], "private notes here");
    assert_eq!(m["origin"], "https://github.com/example/solved.git");
    assert!(!m["unidentified"].as_bool().unwrap_or(false));

    // clearing fields works (empty description clears it)
    let resp = client
        .post(format!("{base}/repos/example-solved/metadata"))
        .form(&[("name", "Mystery Solved"), ("description", ""), ("notes", ""), ("origin", "https://github.com/example/solved.git")])
        .send()
        .await?
        .text()
        .await?;
    assert!(resp.contains("metadata saved"), "{resp}");
    let m: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(archive.join("example-solved/repo.json"))?)?;
    assert!(m["description"].is_null(), "empty field clears: {}", m["description"]);
    assert!(m["notes"].is_null());
    Ok(())
}
