//! End-to-end integration tests using local bare repos as fake remotes.
//! Everything is offline: remotes are `file://` URLs, so shallow fetches
//! behave exactly like they do against real forges.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use reposilo::archiver::{Archiver, SnapshotKind};
use reposilo::config::Config;
use reposilo::index::Index;
use reposilo::types::read_json;

fn git_ok(args: &[&str], cwd: Option<&Path>) {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    let out = cmd.output().expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

const ID: &[&str] = &["-c", "user.email=test@example.com", "-c", "user.name=Test"];

/// Create a bare "remote" under <parent>/remotes/ with one commit on master.
/// Nesting under `remotes/` gives a stable owner segment ("remotes") in the
/// archive tree regardless of where the tempdir lives.
fn make_remote(parent: &Path, name: &str) -> PathBuf {
    let remotes = parent.join("remotes");
    fs::create_dir_all(&remotes).unwrap();
    let remote = remotes.join(format!("{name}.git"));
    git_ok(&["init", "--bare", "-b", "master", remote.to_str().unwrap()], None);

    let work = parent.join(format!("work-{name}"));
    git_ok(&["init", "-b", "master", work.to_str().unwrap()], None);
    fs::write(work.join("README.md"), "hello\n").unwrap();
    fs::write(work.join("main.rs"), "fn main() {}\n").unwrap();
    fs::write(work.join("docs.md"), "notes\n").unwrap();
    git_ok(&["add", "."], Some(&work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", "init"]);
    git_ok(&args, Some(&work));
    git_ok(&["remote", "add", "origin", remote.to_str().unwrap()], Some(&work));
    git_ok(&["push", "origin", "master"], Some(&work));
    remote
}

fn push_commit(work: &Path, file: &str, content: &str, msg: &str) {
    fs::write(work.join(file), content).unwrap();
    git_ok(&["add", "."], Some(work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", msg]);
    git_ok(&args, Some(work));
    git_ok(&["push", "origin", "master"], Some(work));
}

fn push_tag(work: &Path, tag: &str) {
    git_ok(&["tag", tag], Some(work));
    git_ok(&["push", "origin", format!("refs/tags/{tag}").as_str()], Some(work));
}

fn write_manifest(path: &std::path::Path, m: &reposilo::types::RepoManifest) -> std::io::Result<()> {
    std::fs::write(path, serde_json::to_string_pretty(m)?)
}

fn test_cfg(root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.archive.root = root.to_string_lossy().into_owned();
    cfg
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

#[tokio::test]
async fn add_snapshots_branch_and_release() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "fixture");
    push_tag(&tmp.path().join("work-fixture"), "v1.0.0");

    let archiver = Archiver::new(test_cfg(&archive));
    let repo_dir = archiver.add_repo(&file_url(&remote), &["decomps".into(), "n64".into()], None).await?;

    // zip-only storage: NO persistent git store at default depth
    assert!(!repo_dir.join("shallow.git").exists(), "git store must be ephemeral at depth>0");
    // plain README.md extracted next to the metadata (< few KB)
    let plain_readme = fs::read_to_string(repo_dir.join("README.md")).unwrap_or_default();
    assert!(plain_readme.contains("hello"), "README.md extracted: {plain_readme}");
    // manifest
    let manifest_path = repo_dir.join("repo.json");
    assert!(manifest_path.exists());
    let m = read_json::<serde_json::Value>(&manifest_path)?;
    assert_eq!(m["forge"], "generic");
    assert_eq!(m["default_branch"], "master");
    assert_eq!(m["language"], "Rust", "language detected at add time");
    assert_eq!(m["tags"][0], "decomps");

    // branch snapshot zip + sidecar
    let branch_dir = repo_dir.join("branch").join("master");
    let zips: Vec<_> = fs::read_dir(&branch_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "zip"))
        .collect();
    assert_eq!(zips.len(), 1, "exactly one branch snapshot expected");
    assert!(
        zips[0].file_name().unwrap().to_str().unwrap().starts_with("fixture-master@"),
        "zip name must start with the project name: {:?}", zips[0].file_name()
    );
    let sidecar = read_json::<reposilo::types::SnapshotSidecar>(&zips[0].with_extension("json"))?;
    assert_eq!(sidecar.kind, SnapshotKind::Branch.as_str());
    assert_eq!(sidecar.repo, "remotes-fixture"); // owner comes from remotes/ dir
    assert!(!sidecar.commit.is_empty());
    assert_eq!(sidecar.zip.bytes, fs::metadata(&zips[0])?.len());

    // embedded metadata inside the zip makes it self-describing (AIO)
    {
        use std::io::Read;
        let mut z = zip::ZipArchive::new(fs::File::open(&zips[0])?)?;
        let meta_name = z
            .file_names()
            .find(|n| n.ends_with(".reposilo.json"))
            .expect(".reposilo.json embedded in zip")
            .to_string();
        assert!(meta_name.starts_with("fixture/"), "embedded under project prefix: {meta_name}");
        let mut content = String::new();
        z.by_name(&meta_name)?.read_to_string(&mut content)?;
        let v: serde_json::Value = serde_json::from_str(&content)?;
        assert_eq!(v["origin"], file_url(&remote));
        assert_eq!(v["kind"], "branch-snapshot");
        assert_eq!(v["commit"], sidecar.commit);
        assert!(v.get("zip").is_none(), "embedded copy omits the self-referential zip block");
    }

    // release
    let rel_dir = repo_dir.join("releases").join("v1.0.0");
    assert!(rel_dir.join("fixture-v1.0.0.zip").exists());
    assert!(rel_dir.join("fixture-v1.0.0.json").exists());

    // index sees it, filter works
    let index = Index::load(&archive)?;
    assert_eq!(index.repos.len(), 1);
    assert_eq!(index.filter(&["decomps".into(), "n64".into()], None).len(), 1);
    assert_eq!(index.filter(&["decomps".into(), "wrong".into()], None).len(), 0);
    assert_eq!(index.filter(&[], Some("FIXTURE")).len(), 1); // case-insensitive query
    Ok(())
}

#[tokio::test]
async fn refresh_picks_up_new_branch_and_release_and_prunes() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "proj");
    let work = tmp.path().join("work-proj");
    push_tag(&work, "v1.0.0");

    let cfg = test_cfg(&archive);
    let archiver = Archiver::new(cfg.clone());
    archiver.add_repo(&file_url(&remote), &[], None).await?;

    // old files exist
    let repo_dir = archive.join("remotes-proj");
    assert!(repo_dir.join("releases").join("v1.0.0").join("proj-v1.0.0.zip").exists());

    // remote changes: new commit + newer release tag
    push_commit(&work, "new.txt", "new\n", "second");
    push_tag(&work, "v1.1.0");

    let summary = archiver.refresh_repo(&repo_dir).await?;
    assert!(summary.new_branch_snapshot);
    assert_eq!(summary.new_release.as_deref(), Some("1.1.0"));

    // retention keeps only newest branch snapshot and newest release
    assert_eq!(fs::read_dir(repo_dir.join("branch").join("master"))?.count(), 2); // 1 zip + 1 json
    assert!(!repo_dir.join("shallow.git").exists(), "refresh must not leave a git store behind");
    let releases: Vec<_> = fs::read_dir(repo_dir.join("releases"))?.flatten().collect();
    assert_eq!(releases.len(), 1, "old release must be pruned, found {releases:?}");
    assert!(repo_dir.join("releases").join("v1.1.0").join("proj-v1.1.0.zip").exists());

    // refreshing with no changes is a no-op
    let summary2 = archiver.refresh_repo(&repo_dir).await?;
    assert!(!summary2.new_branch_snapshot);
    assert!(summary2.new_release.is_none());
    Ok(())
}

#[tokio::test]
async fn keep_all_retention_keeps_history() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "keeper");
    let work = tmp.path().join("work-keeper");
    push_tag(&work, "v1.0.0");

    let mut cfg = test_cfg(&archive);
    cfg.retention.keep_branch_snapshots = -1;
    cfg.retention.keep_releases = -1;
    let archiver = Archiver::new(cfg);
    archiver.add_repo(&file_url(&remote), &[], None).await?;

    push_commit(&work, "two.txt", "two\n", "two");
    push_tag(&work, "v2.0.0");
    let repo_dir = archive.join("remotes-keeper");
    archiver.refresh_repo(&repo_dir).await?;

    // both branch snapshots kept
    let zips: Vec<_> = fs::read_dir(repo_dir.join("branch").join("master"))?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "zip"))
        .collect();
    assert_eq!(zips.len(), 2);
    // both releases kept
    assert!(repo_dir.join("releases").join("v1.0.0").join("keeper-v1.0.0.zip").exists());
    assert!(repo_dir.join("releases").join("v2.0.0").join("keeper-v2.0.0.zip").exists());
    Ok(())
}

#[tokio::test]
async fn remote_gone_leaves_archive_intact() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "doomed");

    let archiver = Archiver::new(test_cfg(&archive));
    let repo_dir = archiver.add_repo(&file_url(&remote), &["precious".into()], None).await?;

    // simulate remote deletion after 22 days of unavailability (aged timestamp)
    fs::remove_dir_all(&remote)?;
    let mpath = repo_dir.join("repo.json");
    let mut m = read_json::<reposilo::types::RepoManifest>(&mpath)?;
    m.unavailable_since = Some(reposilo::archiver::now_rfc3339());
    let aged = time::OffsetDateTime::now_utc() - time::Duration::days(22);
    m.unavailable_since = Some(aged.format(&time::format_description::well_known::Rfc3339).unwrap());
    write_manifest(&mpath, &m).unwrap();

    // refresh handles the dead remote gracefully: local copy is never touched
    // 22 days > dead_after_days (21) → the remote is declared dead
    let summary = archiver.refresh_repo(&repo_dir).await?;
    assert!(summary.remote_unavailable);
    assert!(!summary.new_branch_snapshot);

    // remote_state recorded on the manifest
    let manifest = read_json::<reposilo::types::RepoManifest>(&repo_dir.join("repo.json"))?;
    assert_eq!(manifest.remote_state.as_deref(), Some("dead"), "22 days of failures = dead");

    // and the archive remains fully intact + indexable
    assert!(repo_dir.join("branch/master").is_dir());
    let index = Index::load(&archive)?;
    assert_eq!(index.filter(&["precious".into()], None).len(), 1);
    Ok(())
}

#[tokio::test]
async fn tar_zst_end_to_end() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "zproj");
    let work = tmp.path().join("work-zproj");
    push_tag(&work, "v1.0.0");

    let mut cfg = test_cfg(&archive);
    cfg.archive.format = "tar.zst".into();
    let archiver = Archiver::new(cfg);
    let repo_dir = archiver.add_repo(&file_url(&remote), &[], None).await?;

    // branch snapshot is a .tar.zst with a sidecar; the sidecar name keeps the
    // full stem (foo.tar.zst -> foo.tar.json), and the index/router must cope
    let branch_dir = repo_dir.join("branch").join("master");
    let archives: Vec<_> = fs::read_dir(&branch_dir)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(".tar.zst"))
        .collect();
    assert_eq!(archives.len(), 1, "one tar.zst branch snapshot");
    let sidecar = read_json::<reposilo::types::SnapshotSidecar>(&archives[0].with_extension("json"))?;
    assert_eq!(sidecar.format.as_deref(), Some("tar.zst"));
    assert!(sidecar.zip.file.ends_with(".tar.zst"));
    assert_eq!(sidecar.zip.bytes, fs::metadata(&archives[0])?.len());

    // the tar.zst archive is readable with the format-detecting helpers
    let readme = reposilo::files::readme_from_archive(&archives[0]).expect("readme in tar.zst");
    assert!(readme.1.contains("hello"), "README extracted from tar.zst");
    let listing = reposilo::files::list_archive(&archives[0]).expect("listing");
    assert!(listing.iter().any(|e| e.name == "README.md"));
    assert!(reposilo::files::find_archive_entry(&archives[0], "main.rs").is_some());

    // release archived in the same format
    let releases: Vec<_> = fs::read_dir(repo_dir.join("releases"))?.flatten().collect();
    assert_eq!(releases.len(), 1, "one release dir");
    let rel_files: Vec<_> = fs::read_dir(releases[0].path())?.flatten().map(|e| e.path()).collect();
    assert!(rel_files.iter().any(|p| p.to_string_lossy().ends_with(".tar.zst")));
    assert!(rel_files.iter().any(|p| p.to_string_lossy().ends_with(".tar.json")));

    // index rebuild sees the snapshot in either format
    let index = Index::load(&archive)?;
    assert_eq!(index.repos.len(), 1);
    assert_eq!(index.snapshot_count(), 2, "branch + release");

    // refresh into the same format still works (and prunes to 1 + 1)
    push_commit(&work, "more.txt", "more\n", "more");
    let summary = archiver.refresh_repo(&repo_dir).await?;
    assert!(summary.new_branch_snapshot);
    Ok(())
}

#[tokio::test]
async fn duplicate_add_is_rejected_even_under_a_folder() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let remote = make_remote(tmp.path(), "dup");
    let archiver = Archiver::new(test_cfg(&archive));
    archiver.add_repo(&file_url(&remote), &[], None).await?;

    // same URL, root location: rejected
    let err = archiver.add_repo(&file_url(&remote), &[], None).await.unwrap_err();
    assert!(err.to_string().contains("already archived"), "{err}");

    // move the repo under a category folder and try again: still rejected
    let repo_dir = archive.join("remotes-dup");
    let dest = archive.join("category").join("remotes-dup");
    fs::create_dir_all(dest.parent().unwrap())?;
    fs::rename(&repo_dir, &dest)?;
    let err = archiver.add_repo(&file_url(&remote), &[], None).await.unwrap_err();
    assert!(err.to_string().contains("already archived"), "{err}");
    assert!(!repo_dir.exists(), "no duplicate directory at the root");
    Ok(())
}

#[tokio::test]
async fn failed_add_leaves_no_partial_repo() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let missing = tmp.path().join("remotes").join("ghost.git");
    let archiver = Archiver::new(test_cfg(&archive));
    let err = archiver.add_repo(&file_url(&missing), &[], None).await.unwrap_err();
    assert!(format!("{err:#}").to_lowercase().contains("clone") || format!("{err:#}").contains("git"));

    // the half-created dir must be gone so a clean retry is possible
    assert!(!archive.join("remotes-ghost").exists(), "partial repo dir must be cleaned up");
    let index = Index::load(&archive)?;
    assert_eq!(index.repos.len(), 0);
    Ok(())
}
/// Content dedup: a release tag pointing at the branch HEAD must hard-link to
/// the branch snapshot (one inode, two paths), for both archive formats. The
/// shared file keeps the original's embedded metadata; each sidecar keeps its
/// own kind/ref/version.
#[cfg(unix)]
#[tokio::test]
async fn same_commit_snapshots_share_one_inode() -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    for format in ["zip", "tar.zst"] {
        let tmp = tempfile::tempdir()?;
        let archive = tmp.path().join("archive");
        fs::create_dir_all(&archive)?;

        let remote = make_remote(tmp.path(), "dedup");
        push_tag(&tmp.path().join("work-dedup"), "v1.0.0"); // tag == branch HEAD

        let mut cfg = test_cfg(&archive);
        cfg.archive.format = format.into();
        let repo_dir = Archiver::new(cfg)
            .add_repo(&file_url(&remote), &[], None)
            .await?;

        let find_one = |dir: &Path| -> PathBuf {
            fs::read_dir(dir)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .find(|p| p.to_string_lossy().ends_with(&format!(".{format}")))
                .unwrap_or_else(|| panic!("no .{format} archive in {}", dir.display()))
        };
        let branch = find_one(&repo_dir.join("branch").join("master"));
        let release = find_one(&repo_dir.join("releases").join("v1.0.0"));

        let (mb, mr) = (fs::metadata(&branch)?, fs::metadata(&release)?);
        assert_eq!(
            mb.ino(),
            mr.ino(),
            "[{format}] same commit must share one hard-linked file"
        );
        assert!(mb.nlink() >= 2, "[{format}] shared file must have >= 2 links");

        // the file's bytes (and thus hash) are identical across both sidecars
        let b_sc = read_json::<reposilo::types::SnapshotSidecar>(&branch.with_extension("json"))?;
        let r_sc = read_json::<reposilo::types::SnapshotSidecar>(&release.with_extension("json"))?;
        assert_eq!(b_sc.commit, r_sc.commit);
        assert_eq!(b_sc.zip.sha256, r_sc.zip.sha256, "[{format}] shared bytes => shared hash");
        // ...yet each sidecar still records what it really is
        assert_eq!(b_sc.kind, "branch-snapshot");
        assert_eq!(r_sc.kind, "release");
        assert_eq!(r_sc.r#ref, "v1.0.0");
        assert_eq!(r_sc.version.as_deref(), Some("1.0.0"));
    }
    Ok(())
}

/// `delete?files=false` unregisters a repo but keeps its snapshots. Re-adding
/// the same URL must work (not be blocked by the leftover folder) and heal.
#[tokio::test]
async fn readd_after_unregister_heals_without_duplicates() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;
    let remote = make_remote(tmp.path(), "reup");
    let archiver = Archiver::new(test_cfg(&archive));
    let repo_dir = archiver.add_repo(&file_url(&remote), &[], None).await?;

    // simulate delete?files=false: drop the manifest, keep the snapshots
    fs::remove_file(repo_dir.join("repo.json"))?;
    assert!(repo_dir.join("branch/master").is_dir());

    let repo_dir2 = archiver.add_repo(&file_url(&remote), &[], None).await?;
    assert_eq!(repo_dir, repo_dir2);
    assert!(repo_dir2.join("repo.json").exists());

    // retention kept exactly one branch snapshot; the stale one was pruned
    let snaps: Vec<_> = fs::read_dir(repo_dir2.join("branch/master"))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("json"))
        .collect();
    assert_eq!(snaps.len(), 1, "old snapshot pruned, no duplicates left");
    Ok(())
}

/// If a re-add over an unregistered orphan fails, its on-disk snapshots must
/// survive (the failure cleanup only removes dirs the add itself created).
#[tokio::test]
async fn failed_readd_over_orphan_preserves_snapshots() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;

    let orphan = archive.join("remotes-phantom").join("branch").join("master");
    fs::create_dir_all(&orphan)?;
    fs::write(orphan.join("precious.txt"), "keep me")?;

    let missing = tmp.path().join("remotes").join("phantom.git");
    let err = Archiver::new(test_cfg(&archive))
        .add_repo(&file_url(&missing), &[], None)
        .await
        .unwrap_err();
    assert!(!format!("{err:#}").is_empty());
    assert!(
        orphan.join("precious.txt").exists(),
        "orphan snapshots must not be deleted by a failed re-add"
    );
    Ok(())
}
