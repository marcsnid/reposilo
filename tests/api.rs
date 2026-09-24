//! HTTP API tests: spin up the real axum server on a random port against a
//! temp archive + local fixture remote, then exercise the whole REST surface.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

use reposilo::archiver::Archiver;
use reposilo::config::Config;
use reposilo::server::{router, AppState};

use std::sync::Arc;

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

fn make_remote(parent: &Path, name: &str) -> (PathBuf, PathBuf) {
    let remotes = parent.join("remotes");
    fs::create_dir_all(&remotes).unwrap();
    let remote = remotes.join(format!("{name}.git"));
    git_ok(&["init", "--bare", "-b", "master", remote.to_str().unwrap()], None);
    let work = parent.join(format!("work-{name}"));
    git_ok(&["init", "-b", "master", work.to_str().unwrap()], None);
    fs::write(work.join("README.md"), "hello\n").unwrap();
    git_ok(&["add", "."], Some(&work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", "init"]);
    git_ok(&args, Some(&work));
    git_ok(&["remote", "add", "origin", remote.to_str().unwrap()], Some(&work));
    git_ok(&["push", "origin", "master"], Some(&work));
    (remote, work)
}

fn push_commit(work: &Path, file: &str, content: &str) {
    fs::write(work.join(file), content).unwrap();
    git_ok(&["add", "."], Some(work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", "more"]);
    git_ok(&args, Some(work));
    git_ok(&["push", "origin", "master"], Some(work));
}

fn push_tag(work: &Path, tag: &str) {
    git_ok(&["tag", tag], Some(work));
    git_ok(&["push", "origin", format!("refs/tags/{tag}").as_str()], Some(work));
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

async fn spawn_server(cfg: Config) -> (String, Arc<AppState>) {
    let st = Arc::new(AppState::new(cfg, None).await.expect("state init"));
    let app = router(st.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), st)
}

async fn wait_job(base: &str, job_id: u64) -> Value {
    let client = reqwest::Client::new();
    for _ in 0..120 {
        let jobs: Value = client
            .get(format!("{base}/api/jobs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let job = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"].as_u64() == Some(job_id))
            .cloned();
        if let Some(j) = job {
            if j["status"] != "running" {
                return j;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("job {job_id} never finished");
}

fn test_cfg(root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.archive.root = root.to_string_lossy().into_owned();
    cfg
}

#[tokio::test]
async fn full_api_lifecycle() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (remote, work) = make_remote(tmp.path(), "apiproj");
    push_tag(&work, "v1.0.0");

    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // empty index
    let list: Value = client.get(format!("{base}/api/repos")).send().await?.json().await?;
    assert_eq!(list["total"], 0);

    // add via job
    let resp: Value = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "url": file_url(&remote), "tags": ["api-test", "fixture"] }))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(resp["job_id"], 1);
    let job = wait_job(&base, 1).await;
    assert_eq!(job["status"], "done", "add job failed: {job}");

    // list: one repo, filterable
    let list: Value = client.get(format!("{base}/api/repos")).send().await?.json().await?;
    assert_eq!(list["total"], 1);
    assert_eq!(list["repos"][0]["path"], "remotes-apiproj");
    assert_eq!(list["repos"][0]["forge"], "generic");
    let filtered: Value = client
        .get(format!("{base}/api/repos?tags=api-test,fixture"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(filtered["total"], 1);
    let filtered: Value = client
        .get(format!("{base}/api/repos?tags=nope"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(filtered["total"], 0);

    // tags endpoint
    let tags: Value = client.get(format!("{base}/api/tags")).send().await?.json().await?;
    assert_eq!(tags["tags"][0]["tag"], "api-test");

    // detail + download
    let detail: Value = client
        .get(format!("{base}/api/repos/remotes-apiproj"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["default_branch"], "master");
    let download: String = detail["branch_snapshots"][0]["download"].as_str().unwrap().to_string();
    let dl = client.get(format!("{base}{download}")).send().await?;
    assert_eq!(dl.status(), 200);
    let bytes = dl.bytes().await?;
    assert!(!bytes.is_empty());

    // PATCH tags
    let patched: Value = client
        .patch(format!("{base}/api/repos/remotes-apiproj"))
        .json(&serde_json::json!({ "tags": ["api-test", "new-tag"], "retention": { "keep_releases": -1, "keep_branch_snapshots": -1 } }))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(patched["tags"][1], "new-tag");

    // 404 for unknown repo
    let r = client.get(format!("{base}/api/repos/nope/missing")).send().await?;
    assert_eq!(r.status(), 404);

    // manual refresh with no remote changes → no new snapshots
    let resp: Value = client
        .post(format!("{base}/api/repos/remotes-apiproj/refresh"))
        .send()
        .await?
        .json()
        .await?;
    let job = wait_job(&base, resp["job_id"].as_u64().unwrap()).await;
    assert_eq!(job["status"], "done");

    // remote changes: new commit + new release → refresh produces notifications
    push_commit(&work, "two.txt", "two\n");
    push_tag(&work, "v2.0.0");
    let resp: Value = client
        .post(format!("{base}/api/repos/remotes-apiproj/refresh"))
        .send()
        .await?
        .json()
        .await?;
    wait_job(&base, resp["job_id"].as_u64().unwrap()).await;

    let notifs: Value = client
        .get(format!("{base}/api/repos/remotes-apiproj/notifications"))
        .send()
        .await?
        .json()
        .await?;
    let msgs: Vec<&str> = notifs["notifications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["title"].as_str().unwrap())
        .collect();
    assert!(msgs.iter().any(|m| m.contains("new release 2.0.0")), "{msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("branch snapshot")), "{msgs:?}");

    // detail reflects both snapshots + both releases (keep_releases = -1)
    let detail: Value = client
        .get(format!("{base}/api/repos/remotes-apiproj"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["branch_snapshot_count"], 2);
    assert_eq!(detail["release_count"], 2);

    // tree endpoint
    let tree: Value = client.get(format!("{base}/api/tree")).send().await?.json().await?;
    assert_eq!(tree["repo_count"], 1);

    // reindex endpoint
    let ri: Value = client.post(format!("{base}/api/reindex")).send().await?.json().await?;
    assert_eq!(ri["repos"], 1);

    // DELETE without files keeps data, removes from index
    let r = client.delete(format!("{base}/api/repos/remotes-apiproj?files=false")).send().await?;
    assert_eq!(r.status(), 204);
    let list: Value = client.get(format!("{base}/api/repos")).send().await?.json().await?;
    assert_eq!(list["total"], 0);
    assert!(tmp.path().join("archive/remotes-apiproj/branch").is_dir(), "files must survive unregister");

    // reindex does NOT resurrect it (no repo.json)
    let ri: Value = client.post(format!("{base}/api/reindex")).send().await?.json().await?;
    assert_eq!(ri["repos"], 0);

    Ok(())
}

#[tokio::test]
async fn add_job_failure_is_reported() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // url that cannot be cloned
    let resp: Value = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "url": "file:///tmp/reposilo-definitely-missing-remote" }))
        .send()
        .await?
        .json()
        .await?;
    let job = wait_job(&base, resp["job_id"].as_u64().unwrap()).await;
    assert_eq!(job["status"], "failed", "expected failure: {job}");
    assert!(job["error"].as_str().unwrap().contains("clone"));

    // bad request: no url
    let r = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "tags": [] }))
        .send()
        .await?;
    assert_eq!(r.status(), 400);
    Ok(())
}

#[tokio::test]
async fn tar_zst_download_endpoint() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (remote, _work) = make_remote(tmp.path(), "zdl");
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;
    let mut cfg = test_cfg(&archive);
    cfg.archive.format = "tar.zst".into();
    Archiver::new(cfg.clone())
        .add_repo(&file_url(&remote), &[], None)
        .await?;
    let (base, _st) = spawn_server(cfg).await;
    let client = reqwest::Client::new();

    let detail: Value = client
        .get(format!("{base}/api/repos/remotes-zdl"))
        .send()
        .await?
        .json()
        .await?;
    let download = detail["branch_snapshots"][0]["download"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(download.ends_with(".tar.zst"), "{download}");

    let dl = client.get(format!("{base}{download}")).send().await?;
    assert_eq!(dl.status(), 200, "tar.zst must be downloadable");
    assert_eq!(
        dl.headers().get("content-type").unwrap(),
        "application/zstd"
    );
    let bytes = dl.bytes().await?;
    assert!(!bytes.is_empty());
    Ok(())
}
