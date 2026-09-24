//! Web UI tests: server-rendered pages, htmx fragment swapping, README
//! rendering, file browsing, tag editing, and the add-repo form flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

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

fn div_balance(html: &str) -> (usize, usize) {
    let opens = html.matches("<div").count();
    let closes = html.matches("</div>").count();
    (opens, closes)
}

// Regression: a stray </div> once closed #app early, so htmx fragment swaps
// appended a second sidebar/grid below the old one ("mirroring" bug).
#[test]
fn templates_render_balanced_divs() {
    for (name, tpl) in [
        ("index", include_str!("../templates/index.html")),
        ("app", include_str!("../templates/app.html")),
        ("fragment", include_str!("../templates/fragment.html")),
        ("detail", include_str!("../templates/repo_detail.html")),
        ("blob", include_str!("../templates/blob.html")),
        ("tags", include_str!("../templates/tags_edit.html")),
        ("folder_form", include_str!("../templates/folder_form.html")),
        ("job", include_str!("../templates/job_status.html")),
        ("settings", include_str!("../templates/settings.html")),
        ("stats", include_str!("../templates/stats.html")),
        ("base", include_str!("../templates/base.html")),
    ] {
        let (o, c) = div_balance(tpl);
        assert_eq!(
            o, c,
            "{name}.html has unbalanced divs ({} open vs {} close): a stray </div> breaks the #app swap target",
            o, c
        );
    }
}

fn make_remote(parent: &Path, name: &str) -> PathBuf {
    let remotes = parent.join("remotes");
    fs::create_dir_all(&remotes).unwrap();
    let remote = remotes.join(format!("{name}.git"));
    git_ok(&["init", "--bare", "-b", "master", remote.to_str().unwrap()], None);
    let work = parent.join(format!("work-{name}"));
    git_ok(&["init", "-b", "master", work.to_str().unwrap()], None);
    fs::write(work.join("README.md"), "# WebTest\n\nA fixture repo for the web UI tests.\n".as_bytes()).unwrap();
    fs::write(work.join("hello.txt"), "line one\nline two\n").unwrap();
    fs::create_dir(work.join("src")).unwrap();
    fs::write(work.join("src/main.rs"), "fn main() {}\n").unwrap();
    git_ok(&["add", "."], Some(&work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", "init"]);
    git_ok(&args, Some(&work));
    git_ok(&["remote", "add", "origin", remote.to_str().unwrap()], Some(&work));
    git_ok(&["push", "origin", "master"], Some(&work));
    remote
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

async fn wait_jobs_done(base: &str) {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        let jobs: serde_json::Value = client
            .get(format!("{base}/api/jobs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let running = jobs["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["status"] == "running");
        if !running && !jobs["jobs"].as_array().unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("jobs never finished");
}

fn test_cfg(root: &Path) -> Config {
    let mut cfg = Config::default();
    cfg.archive.root = root.to_string_lossy().into_owned();
    cfg
}

#[tokio::test]
async fn web_ui_full_flow() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "webproj");

    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // static assets
    let css = client.get(format!("{base}/assets/style.css")).send().await?;
    assert_eq!(css.status(), 200);
    let htmx = client.get(format!("{base}/assets/htmx.min.js")).send().await?;
    assert_eq!(htmx.status(), 200);

    // index page (empty state)
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("reposilo"));
    assert!(page.contains("No repositories match"));
    assert!(page.contains("Popular tags"));
    assert!(page.contains("data-theme")); // theme support present
    // the rendered page must have balanced divs and the layout INSIDE #app
    let (o, c) = div_balance(&page);
    assert_eq!(o, c, "rendered index page has unbalanced divs");
    let app_open = page.find("id=\"app\"").expect("#app in page");
    let layout = page.find("class=\"layout\"").expect("layout in page");
    assert!(layout > app_open, "layout must come after #app opens");

    // add repo via the HTML form (form-encoded, like htmx sends)
    let resp = client
        .post(format!("{base}/repos/add"))
        .form(&[("url", file_url(&remote).as_str()), ("tags", "webtest,fixture")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await?;
    assert!(body.contains("Archiving"), "{body}");
    let job_id: u64 = body
        .split("/repos/job-status/")
        .nth(1)
        .and_then(|s| s.split(['"', '?', '<']).next())
        .and_then(|s| s.parse().ok())
        .expect("job id in response");

    wait_jobs_done(&base).await;

    // polling endpoint shows done + links to the repo
    let status = client
        .get(format!("{base}/repos/job-status/{job_id}"))
        .send()
        .await?
        .text()
        .await?;
    assert!(status.contains("Archived"), "{status}");
    assert!(status.contains("/repos/remotes-webproj"), "{status}");
    assert!(status.contains("hx-swap-oob=\"outerHTML\""), "{status}");

    // index now shows the card, with description auto-derived from README
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("webproj"));
    assert!(page.contains("A fixture repo for the web UI tests."), "description from README: {page}");
    assert!(page.contains("#webtest"));
    assert!(page.contains("lang-rust") && page.contains("Rust"), "language dot on card: {page}");

    // filter via sidebar tag (query param; fragment swap uses same URL)
    let page = client.get(format!("{base}/?tags=webtest,fixture")).send().await?.text().await?;
    assert!(page.contains("webproj"));
    let page = client.get(format!("{base}/?tags=nope")).send().await?.text().await?;
    assert!(page.contains("No repositories match"));

    // htmx fragment request (hx-request header) returns only the app fragment
    let frag = client
        .get(format!("{base}/?tags=webtest"))
        .header("hx-request", "true")
        .send()
        .await?
        .text()
        .await?;
    assert!(!frag.contains("<!DOCTYPE html>"), "fragment must not be a full page");
    assert!(frag.contains("webproj"));
    assert!(frag.contains("class=\"chips active-chips\""));

    // detail page: README rendered as HTML, file listing, tags editor
    let detail = client.get(format!("{base}/repos/remotes-webproj")).send().await?.text().await?;
    assert!(detail.contains("<h1>WebTest</h1>"), "readme markdown rendered");
    assert!(detail.contains("hello.txt"), "root file listed");
    assert!(detail.contains("tag-editor"), "tag editor present");
    assert!(detail.contains("keep newest 1 release"), "retention shown");

    // blob view: grab the zip-based URL straight from the detail page
    let detail = client
        .get(format!("{base}/repos/remotes-webproj"))
        .send()
        .await?
        .text()
        .await?;
    let blob_url = detail
        .split("href=\"")
        .find(|u| u.contains("/blob/") && u.contains("hello.txt"))
        .and_then(|u| u.split('"').next().map(str::to_string))
        .expect("blob link on detail page");
    // path traversal attempts are rejected (router normalizes `..` to a 404,
    // and even a direct hit only reaches central-directory entries)
    let evil = format!("{base}/repos/remotes-webproj/blob/../../repo.json/evil");
    let blob = client.get(evil).send().await?;
    assert!(blob.status() == 400 || blob.status() == 404, "traversal must be rejected");
    let evil2 = format!("{base}/repos/remotes-webproj/blob/no-such-zip.zip/../../repo.json");
    let blob = client.get(evil2).send().await?;
    assert!(blob.status() == 400 || blob.status() == 404, "traversal must be rejected");
    let blob = client
        .get(format!("{base}{blob_url}"))
        .send()
        .await?;
    assert_eq!(blob.status(), 200);
    let blob_text = blob.text().await?;
    assert!(blob_text.contains("line one"), "{blob_text}");
    assert!(blob_text.contains("<pre"), "text file shown as pre");

    // markdown blob renders (same snapshot, different file)
    let md_url = blob_url.replace("hello.txt", "README.md");
    let blob = client
        .get(format!("{base}{md_url}"))
        .send()
        .await?
        .text()
        .await?;
    assert!(blob.contains("<h1>WebTest</h1>"));

    // tag add via the form endpoint: the client posts only the edit;
    // the manifest on disk is the tag list of record.
    let resp = client
        .post(format!("{base}/repos/remotes-webproj/tags"))
        .form(&[("op", "add"), ("value", "added-tag")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let body = resp.text().await?;
    assert!(body.contains("#added-tag"), "{body}");

    // regression: a second add must accumulate, not overwrite (the old form
    // carried a stale client-side tag list that clobbered the first add)
    let body = client
        .post(format!("{base}/repos/remotes-webproj/tags"))
        .form(&[("op", "add"), ("value", "another-tag")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("#added-tag") && body.contains("#another-tag"), "{body}");

    // tag remove
    let body = client
        .post(format!("{base}/repos/remotes-webproj/tags"))
        .form(&[("op", "remove"), ("value", "added-tag")])
        .send()
        .await?
        .text()
        .await?;
    assert!(!body.contains("#added-tag"), "{body}");

    // and the manifest on disk reflects it (disk is the truth)
    let manifest: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        tmp.path().join("archive/remotes-webproj/repo.json"),
    )?)?;
    let tags: Vec<&str> = manifest["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap()).collect();
    assert_eq!(tags, ["another-tag", "fixture", "webtest"], "tags sorted, added-tag removed");

    Ok(())
}
#[tokio::test]
async fn folder_management_flow() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "webproj");

    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // add a repo first
    client
        .post(format!("{base}/repos/add"))
        .form(&[("url", file_url(&remote).as_str()), ("tags", "webtest")])
        .send()
        .await?;
    wait_jobs_done(&base).await;

    // create a folder via the GUI endpoint
    let resp = client
        .post(format!("{base}/folders/create"))
        .form(&[("parent", ""), ("name", "games"), ("icon", "🎮")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    // folder.json on disk carries the icon
    let fm: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        tmp.path().join("archive/games/folder.json"),
    )?)?;
    assert_eq!(fm["icon"], "🎮");
    // empty folder is visible in the sidebar with count 0 and its icon
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("games"), "{page}");
    assert!(page.contains("🎮"), "{page}");

    // nested folder: decomp under games
    let resp = client
        .post(format!("{base}/folders/create"))
        .form(&[("parent", "games"), ("name", "decomp"), ("icon", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/games/decomp/folder.json").exists());

    // move the repo into games/decomp via the metadata form
    let resp = client
        .post(format!("{base}/repos/remotes-webproj/metadata"))
        .form(&[("folder", "games/decomp"), ("name", "WebTest"), ("description", ""), ("notes", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(
        tmp.path().join("archive/games/decomp/remotes-webproj/repo.json").exists(),
        "repo moved on disk"
    );
    // sidebar shows the nested folder with count 1
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("decomp"), "{page}");

    // rename the inner folder
    let resp = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/decomp"), ("op", "save"), ("name", "n64"), ("icon", "🕹")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/games/n64/remotes-webproj/repo.json").exists());
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("🕹"), "{page}");

    // delete with a repo inside must fail
    let body = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/n64"), ("op", "delete"), ("name", "n64"), ("icon", "")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("move them out first"), "{body}");
    assert!(tmp.path().join("archive/games/n64").exists());

    // move the repo back out, then delete succeeds
    client
        .post(format!("{base}/repos/games/n64/remotes-webproj/metadata"))
        .form(&[("folder", ""), ("name", "WebTest"), ("description", ""), ("notes", "")])
        .send()
        .await?;
    assert!(tmp.path().join("archive/remotes-webproj/repo.json").exists());
    let resp = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/n64"), ("op", "delete"), ("name", "n64"), ("icon", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(!tmp.path().join("archive/games/n64").exists());

    // invalid names are rejected
    let body = client
        .post(format!("{base}/folders/create"))
        .form(&[("parent", ""), ("name", "a/b"), ("icon", "")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("invalid folder name"), "{body}");

    Ok(())
}

#[tokio::test]
async fn drag_move_and_add_with_folder() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "webproj");

    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // add with a folder: the job places the repo under it when it finishes
    client
        .post(format!("{base}/repos/add"))
        .form(&[
            ("url", file_url(&remote).as_str()),
            ("tags", "webtest"),
            ("folder", "tools/cli"),
        ])
        .send()
        .await?;
    wait_jobs_done(&base).await;
    assert!(
        tmp.path().join("archive/tools/cli/remotes-webproj/repo.json").exists(),
        "add with folder places the repo there"
    );

    // folder options endpoint lists the implicit folder chain
    let opts = client.get(format!("{base}/folders/options")).send().await?.text().await?;
    assert!(opts.contains("tools"), "{opts}");
    assert!(opts.contains("tools/cli"), "{opts}");

    // drag-and-drop move: tools/cli -> tools (one level up)
    let resp = client
        .post(format!("{base}/repos/move"))
        .form(&[("rel", "tools/cli/remotes-webproj"), ("folder", "tools")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/tools/remotes-webproj/repo.json").exists());
    assert!(!tmp.path().join("archive/tools/cli/remotes-webproj").exists());

    // drop on "All repositories" (empty folder) moves back to the root
    let resp = client
        .post(format!("{base}/repos/move"))
        .form(&[("rel", "tools/remotes-webproj"), ("folder", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/remotes-webproj/repo.json").exists());

    // a folder path running through another repo is rejected
    let body = client
        .post(format!("{base}/repos/move"))
        .form(&[("rel", "remotes-webproj"), ("folder", "remotes-webproj/sub")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("invalid folder path"), "{body}");

    Ok(())
}

/// Repo deletion is a UI flow: a confirm step plus an explicit "also delete
/// files" check. Unchecked = unregister only (snapshots stay on disk).
#[tokio::test]
async fn repo_delete_ui_confirms_then_deletes() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "delproj");
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;
    let (base, _st) = spawn_server(test_cfg(&archive)).await;
    let client = reqwest::Client::new();

    let _: serde_json::Value = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "url": file_url(&remote) }))
        .send()
        .await?
        .json()
        .await?;
    wait_jobs_done(&base).await;

    // the detail page offers a Danger zone delete
    let detail = client
        .get(format!("{base}/repos/remotes-delproj"))
        .send()
        .await?
        .text()
        .await?;
    assert!(detail.contains("Danger zone"), "{detail}");
    assert!(detail.contains("/repos/remotes-delproj/delete"), "{detail}");

    // the confirm fragment asks, and offers the extra file-delete check
    let confirm = client
        .get(format!("{base}/repos/remotes-delproj/delete"))
        .send()
        .await?;
    assert_eq!(confirm.status(), 200);
    let body = confirm.text().await?;
    assert!(body.contains("also permanently delete the archive files"), "{body}");
    assert!(body.contains("name=\"files\""), "{body}");

    // POST without the check => unregister only, snapshot files stay
    let del = client
        .post(format!("{base}/repos/remotes-delproj/delete"))
        .form(&[("files", "0")])
        .send()
        .await?;
    assert_eq!(del.status(), 200);
    let gone = client
        .get(format!("{base}/repos/remotes-delproj"))
        .send()
        .await?;
    assert_eq!(gone.status(), 404, "repo must leave the index");
    assert!(!archive.join("remotes-delproj").join("repo.json").exists());
    assert!(
        archive.join("remotes-delproj").join("branch").exists(),
        "files must be kept when the box is unchecked"
    );
    Ok(())
}

/// With the extra check ticked, the archive directory is removed entirely.
#[tokio::test]
async fn repo_delete_ui_with_files_removes_the_directory() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "purgeproj");
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;
    let (base, _st) = spawn_server(test_cfg(&archive)).await;
    let client = reqwest::Client::new();

    let _: serde_json::Value = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "url": file_url(&remote) }))
        .send()
        .await?
        .json()
        .await?;
    wait_jobs_done(&base).await;
    assert!(archive.join("remotes-purgeproj").is_dir());

    let del = client
        .post(format!("{base}/repos/remotes-purgeproj/delete"))
        .form(&[("files", "1")])
        .send()
        .await?;
    assert_eq!(del.status(), 200);
    assert!(
        !archive.join("remotes-purgeproj").exists(),
        "checking the box must delete the archive files"
    );
    Ok(())
}

/// Untrusted file contents must never render as executable HTML in the blob
/// viewer (stored XSS from an archived repo).
#[tokio::test]
async fn blob_viewer_escapes_untrusted_html() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let remote = make_remote(tmp.path(), "xssproj");
    // push an untrusted HTML file so it lands in the snapshot
    let work = tmp.path().join("work-xssproj");
    fs::write(work.join("evil.html"), "<script>alert('xss')</script>\n")?;
    git_ok(&["add", "."], Some(&work));
    let mut args = ID.to_vec();
    args.extend(["commit", "-m", "add evil"]);
    git_ok(&args, Some(&work));
    git_ok(&["push", "origin", "master"], Some(&work));
    let (base, _st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    let _: serde_json::Value = client
        .post(format!("{base}/api/repos"))
        .json(&serde_json::json!({ "url": file_url(&remote) }))
        .send()
        .await?
        .json()
        .await?;
    wait_jobs_done(&base).await;

    let detail: serde_json::Value = client
        .get(format!("{base}/api/repos/remotes-xssproj"))
        .send()
        .await?
        .json()
        .await?;
    let file = detail["branch_snapshots"][0]["file"].as_str().unwrap().to_string();

    let page = client
        .get(format!("{base}/repos/remotes-xssproj/blob/{file}/evil.html"))
        .send()
        .await?;
    assert_eq!(page.status(), 200);
    let html = page.text().await?;
    assert!(!html.contains("<script>alert"), "raw script survived: {html}");
    assert!(html.contains("alert("), "file text should still be shown: {html}");
    Ok(())
}
