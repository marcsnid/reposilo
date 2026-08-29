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