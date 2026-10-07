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
        ("verify", include_str!("../templates/verify_status.html")),
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
    let created = resp.text().await?;
    assert!(
        created.contains("hx-swap-oob=\"delete\""),
        "a successful create must close the modal: {created}"
    );
    // folder.json on disk carries the icon
    let fm: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        tmp.path().join("archive/games/folder.json"),
    )?)?;
    assert_eq!(fm["icon"], "🎮");
    // empty folder is visible in the sidebar with count 0 and its icon
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("games"), "{page}");
    assert!(page.contains("🎮"), "{page}");

    // the edit form must carry the folder path so save/rename works (regression:
    // the hidden `rel` field was missing, so updates failed with "no such folder")
    let form = client
        .get(format!("{base}/folders/edit?rel=games"))
        .send()
        .await?
        .text()
        .await?;
    assert!(form.contains(r#"name="rel" value="games""#), "edit form must resubmit rel: {form}");
    assert!(form.contains("modal-backdrop"), "folder edit should be a modal: {form}");
    assert!(form.contains("id=\"folder-modal\""), "{form}");
    assert!(form.contains("id=\"folder-form-error\""), "errors stay inside the modal: {form}");

    // a new folder defaults its parent to the folder currently open
    let new_form = client
        .get(format!("{base}/folders/new?parent=games"))
        .send()
        .await?
        .text()
        .await?;
    assert!(
        new_form.contains(r#"<option value="games" selected>"#),
        "the open folder should be preselected as parent: {new_form}"
    );

    // an icon longer than one character is trimmed to a single grapheme
    let resp = client
        .post(format!("{base}/folders/create"))
        .form(&[("parent", ""), ("name", "wordy"), ("icon", "hello")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    let wm: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(tmp.path().join("archive/wordy/folder.json"))?)?;
    assert_eq!(wm["icon"], "h");

    // nested folder: cli under games
    let resp = client
        .post(format!("{base}/folders/create"))
        .form(&[("parent", "games"), ("name", "cli"), ("icon", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/games/cli/folder.json").exists());

    // move the repo into games/cli via the metadata form
    let resp = client
        .post(format!("{base}/repos/remotes-webproj/metadata"))
        .form(&[("folder", "games/cli"), ("name", "WebTest"), ("description", ""), ("notes", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(
        tmp.path().join("archive/games/cli/remotes-webproj/repo.json").exists(),
        "repo moved on disk"
    );
    // sidebar shows the nested folder with count 1
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("cli"), "{page}");

    // rename the inner folder
    let resp = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/cli"), ("op", "save"), ("name", "build"), ("icon", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(tmp.path().join("archive/games/build/remotes-webproj/repo.json").exists());
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("build"), "{page}");

    // delete with a repo inside must fail
    let body = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/build"), ("op", "delete"), ("name", "build"), ("icon", "")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("move them out first"), "{body}");
    assert!(tmp.path().join("archive/games/build").exists());

    // move the repo back out, then delete succeeds
    client
        .post(format!("{base}/repos/games/build/remotes-webproj/metadata"))
        .form(&[("folder", ""), ("name", "WebTest"), ("description", ""), ("notes", "")])
        .send()
        .await?;
    assert!(tmp.path().join("archive/remotes-webproj/repo.json").exists());
    let resp = client
        .post(format!("{base}/folders/update"))
        .form(&[("rel", "games/build"), ("op", "delete"), ("name", "build"), ("icon", "")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);
    assert!(!tmp.path().join("archive/games/build").exists());

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

    // the detail page offers an Actions panel with delete + force refresh
    let detail = client
        .get(format!("{base}/repos/remotes-delproj"))
        .send()
        .await?
        .text()
        .await?;
    assert!(detail.contains("Actions"), "{detail}");
    assert!(detail.contains("Force Refresh"), "{detail}");
    assert!(detail.contains("/repos/remotes-delproj/delete"), "{detail}");

    // the confirm modal asks for the repo name and offers the extra file check
    let confirm = client
        .get(format!("{base}/repos/remotes-delproj/delete"))
        .send()
        .await?;
    assert_eq!(confirm.status(), 200);
    let body = confirm.text().await?;
    assert!(body.contains("modal-backdrop"), "{body}");
    assert!(body.contains("Are you sure you want to delete"), "{body}");
    assert!(body.contains("This is irreversible."), "{body}");
    assert!(body.contains("name=\"confirm\""), "{body}");
    assert!(body.contains("Permanently delete the archive files"), "{body}");
    assert!(body.contains("name=\"files\""), "{body}");

    // a wrong/missing typed name must be refused and must not touch the repo
    let bad = client
        .post(format!("{base}/repos/remotes-delproj/delete"))
        .form(&[("files", "0"), ("confirm", "not-the-name")])
        .send()
        .await?;
    assert_eq!(bad.status(), 200);
    assert!(bad.text().await?.contains("Type the repository name exactly"), "mismatch refused");
    assert!(archive.join("remotes-delproj").join("repo.json").exists(), "repo must survive a mismatch");

    // POST with the typed name, no file check => unregister only, files stay
    let del = client
        .post(format!("{base}/repos/remotes-delproj/delete"))
        .form(&[("files", "0"), ("confirm", "delproj")])
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
        .form(&[("files", "1"), ("confirm", "purgeproj")])
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

#[tokio::test]
async fn settings_platform_filters_roundtrip() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (base, st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    let html = client.get(format!("{base}/settings")).send().await?.text().await?;
    assert!(html.contains("Release binaries"), "settings should have the panel");
    assert!(
        html.contains("name=\"plat_linux-x64\""),
        "settings should offer the linux-x64 checkbox"
    );

    // select via checkboxes plus free-text; "win64" must canonicalize
    let resp = client
        .post(format!("{base}/settings"))
        .form(&[
            ("plat_darwin-arm64", "1"),
            ("plat_linux-x64", "1"),
            ("release_platforms_extra", "win64, riscv64"),
        ])
        .send()
        .await?;
    assert!(resp.status().is_success());

    let platforms = st.cfg().await.releases.platforms.clone();
    assert_eq!(platforms, vec!["darwin-arm64", "linux-x64", "windows-x64", "riscv64"]);

    // and the page reflects the saved selection
    let html2 = client.get(format!("{base}/settings")).send().await?.text().await?;
    assert!(
        html2.contains("name=\"plat_darwin-arm64\" value=\"1\" checked"),
        "saved checkboxes should render checked: {html2}"
    );
    Ok(())
}

#[tokio::test]
async fn settings_verify_toggle_roundtrip() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let (base, st) = spawn_server(test_cfg(&tmp.path().join("archive"))).await;
    let client = reqwest::Client::new();

    // Off by default.
    let html = client.get(format!("{base}/settings")).send().await?.text().await?;
    assert!(html.contains("Scheduled checks"), "settings should offer the toggle");
    assert!(
        !html.contains("name=\"verify_enabled\" value=\"1\" checked"),
        "scheduled checks must default to unchecked: {html}"
    );

    // Turn it on with an interval.
    let resp = client
        .post(format!("{base}/settings"))
        .form(&[("verify_enabled", "1"), ("verify_interval_days", "30")])
        .send()
        .await?;
    assert!(resp.status().is_success());
    let cfg = st.cfg().await;
    assert!(cfg.verify.enabled);
    assert_eq!(cfg.verify.interval_days, 30);

    // The page reflects the saved value.
    let html2 = client.get(format!("{base}/settings")).send().await?.text().await?;
    assert!(
        html2.contains("name=\"verify_enabled\" value=\"1\" checked"),
        "saved toggle should render checked: {html2}"
    );

    // Posting without the checkbox turns it back off.
    let resp = client
        .post(format!("{base}/settings"))
        .form(&[("verify_interval_days", "30")])
        .send()
        .await?;
    assert!(resp.status().is_success());
    assert!(!st.cfg().await.verify.enabled);
    Ok(())
}

/// A release sidecar that already records a downloaded asset must show up on
/// the repo page, in the API, and be downloadable straight from disk.
#[tokio::test]
async fn stored_release_assets_are_listed_and_downloadable() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    let rel = repo.join("releases").join("v1.0.0");
    fs::create_dir_all(rel.join("assets"))?;

    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    fs::write(
        rel.join("demo-v1.0.0.json"),
        r#"{
          "kind":"release","repo":"owner-demo","origin":"https://github.com/owner/demo",
          "ref":"v1.0.0","version":"1.0.0","commit":"0123456789abcdef0123456789abcdef01234567",
          "archived_at":"2025-01-02T00:00:00Z","archiver_version":"test","format":"zip",
          "assets":[{"name":"demo-linux-x64.tar.gz","platform":"linux-x64","url":"https://example/x","bytes":5,"sha256":"aa","downloaded_at":"2025-01-02T00:00:00Z"}],
          "zip":{"file":"demo-v1.0.0.zip","bytes":0,"sha256":"bb"}
        }"#,
    )?;
    fs::write(rel.join("assets").join("demo-linux-x64.tar.gz"), b"hello")?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let page = client.get(format!("{base}/repos/owner-demo")).send().await?;
    assert_eq!(page.status(), 200);
    let html = page.text().await?;
    assert!(html.contains("demo-linux-x64.tar.gz"), "asset not listed: {html}");
    assert!(html.contains("Linux · x86_64"), "platform label missing: {html}");

    let detail: serde_json::Value = client
        .get(format!("{base}/api/repos/owner-demo"))
        .send()
        .await?
        .json()
        .await?;
    assert_eq!(detail["releases"][0]["assets"][0]["platform"], "linux-x64");

    let asset = client
        .get(format!(
            "{base}/api/repos/owner-demo/asset/releases/v1.0.0/assets/demo-linux-x64.tar.gz"
        ))
        .send()
        .await?;
    assert_eq!(asset.status(), 200);
    assert_eq!(asset.bytes().await?.as_ref(), b"hello");

    // the asset route refuses files outside releases/<tag>/assets/
    let blocked = client
        .get(format!("{base}/api/repos/owner-demo/asset/repo.json"))
        .send()
        .await?;
    assert_eq!(blocked.status(), 404);
    Ok(())
}

/// A repo nested under a folder named `archive` (or `asset`) must still be
/// routable: the download handlers must not blindly split on the first marker.
#[tokio::test]
async fn repos_under_an_archive_folder_still_route() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("cat").join("archive").join("owner-demo");
    let rel = repo.join("releases").join("v1.0.0");
    fs::create_dir_all(rel.join("assets"))?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    fs::write(
        rel.join("demo-v1.0.0.json"),
        r#"{"kind":"release","repo":"cat/archive/owner-demo","origin":"https://github.com/owner/demo","ref":"v1.0.0","version":"1.0.0","commit":"abc","archived_at":"2025-01-02T00:00:00Z","archiver_version":"test","format":"zip","zip":{"file":"demo-v1.0.0.zip","bytes":2,"sha256":"bb"}}"#,
    )?;
    fs::write(rel.join("demo-v1.0.0.zip"), b"PK")?;
    fs::write(rel.join("assets").join("demo-linux-x64.tar.gz"), b"hello")?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();
    let repo_rel = "cat/archive/owner-demo";

    let detail = client
        .get(format!("{base}/api/repos/{repo_rel}"))
        .send()
        .await?;
    assert_eq!(detail.status(), 200, "detail route mis-split the repo path");

    let asset = client
        .get(format!(
            "{base}/api/repos/{repo_rel}/asset/releases/v1.0.0/assets/demo-linux-x64.tar.gz"
        ))
        .send()
        .await?;
    assert_eq!(asset.status(), 200);
    assert_eq!(asset.bytes().await?.as_ref(), b"hello");

    let archive = client
        .get(format!(
            "{base}/api/repos/{repo_rel}/archive/releases/v1.0.0/demo-v1.0.0.zip"
        ))
        .send()
        .await?;
    assert_eq!(archive.status(), 200);
    assert_eq!(archive.bytes().await?.as_ref(), b"PK");
    Ok(())
}

fn extract_verify_id(html: &str) -> u64 {
    let marker = "hx-get=\"/settings/verify/";
    let start = html.find(marker).expect("verify fragment carries a job id") + marker.len();
    let rest = &html[start..];
    let end = rest.find('"').expect("closing quote after job id");
    rest[..end].parse().expect("numeric job id")
}

async fn wait_verify_done(base: &str, id: u64) -> String {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        let html = client
            .get(format!("{base}/settings/verify/{id}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if !html.contains("hx-trigger=\"every 1s\"") {
            return html;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("verify job {id} never finished");
}

/// The Settings button runs a full verify and reports the result.
#[tokio::test]
async fn settings_verify_button_reports_integrity() -> Result<()> {
    // sha256("hello")
    const HELLO_SHA: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    let rel = repo.join("releases").join("v1.0.0");
    fs::create_dir_all(&rel)?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    fs::write(
        rel.join("demo-v1.0.0.json"),
        format!(
            r#"{{"kind":"release","repo":"owner-demo","origin":"https://github.com/owner/demo","ref":"v1.0.0","version":"1.0.0","commit":"abc","archived_at":"2025-01-02T00:00:00Z","archiver_version":"test","format":"zip","zip":{{"file":"demo-v1.0.0.zip","bytes":5,"sha256":"{HELLO_SHA}"}}}}"#
        ),
    )?;
    fs::write(rel.join("demo-v1.0.0.zip"), b"hello")?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let settings = client.get(format!("{base}/settings")).send().await?.text().await?;
    assert!(settings.contains("Archive integrity"), "settings should offer verify");
    assert!(settings.contains("hx-post=\"/settings/verify\""));
    // verify progress is surfaced in the bottom toast
    assert!(settings.contains("hx-post=\"/settings/verify\" hx-target=\"#toast\""), "{settings}");

    // clean archive -> all good
    let first = client
        .post(format!("{base}/settings/verify"))
        .send()
        .await?
        .text()
        .await?;
    assert!(first.contains("spinner"), "running verify should show the spinner: {first}");
    assert!(
        first.contains("id=\"verify-area\" hx-swap-oob=\"innerHTML\""),
        "the report area must be updated out-of-band: {first}"
    );
    let report = wait_verify_done(&base, extract_verify_id(&first)).await;
    assert!(report.contains("Verified 1"), "expected a clean report: {report}");
    assert!(report.contains("No problems found"), "expected the clean note: {report}");
    assert!(!report.contains("badge danger"), "clean report should have no problem rows: {report}");

    // corrupt the zip -> the next verify flags it
    fs::write(rel.join("demo-v1.0.0.zip"), b"tampered!")?;
    let second = client
        .post(format!("{base}/settings/verify"))
        .send()
        .await?
        .text()
        .await?;
    let report = wait_verify_done(&base, extract_verify_id(&second)).await;
    assert!(report.contains("problem"), "expected problems: {report}");
    assert!(report.contains("sha256") || report.contains("size"), "expected a hash/size flag: {report}");
    Ok(())
}

/// "Add all suggested" must adopt every forge/LLM suggestion at once while
/// keeping the repo's own tags.
#[tokio::test]
async fn add_all_suggested_tags_merges_them() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    fs::create_dir_all(&repo)?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main","tags":["manual"],"suggested_tags":["cli","rust","tooling"]}"#,
    )?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    // before: the purple "Add all" pill leads the suggested row
    let before = client.get(format!("{base}/repos/owner-demo")).send().await?.text().await?;
    assert!(before.contains("suggested-all"), "expected an Add all pill: {before}");
    assert!(before.contains(">Add all</button>"), "{before}");

    let body = client
        .post(format!("{base}/repos/owner-demo/tags"))
        .form(&[("op", "add_suggested")])
        .send()
        .await?
        .text()
        .await?;
    assert!(body.contains("#cli"), "{body}");
    assert!(body.contains("#rust"), "{body}");
    assert!(body.contains("#tooling"), "{body}");
    assert!(!body.contains("suggested-all"), "suggestions should all be adopted: {body}");

    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("repo.json"))?)?;
    let tags: Vec<String> =
        m["tags"].as_array().unwrap().iter().map(|t| t.as_str().unwrap().to_string()).collect();
    assert_eq!(tags, vec!["cli", "manual", "rust", "tooling"]);
    Ok(())
}

/// The gallery offers grid/list view toggles (rendered server-side; the
/// choice itself is client-side via localStorage).
#[tokio::test]
async fn index_offers_grid_and_list_view_toggles() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let archive = tmp.path().join("archive");
    fs::create_dir_all(&archive)?;
    let (base, _st) = spawn_server(test_cfg(&archive)).await;
    let client = reqwest::Client::new();

    let html = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(html.contains("class=\"view-toggle\""), "{html}");
    assert!(html.contains("data-view=\"grid\""), "{html}");
    assert!(html.contains("data-view=\"list\""), "{html}");
    assert!(html.contains("setView('list')"), "{html}");

    // the swapped fragment carries the toggles too
    let frag = client
        .get(format!("{base}/"))
        .header("HX-Request", "true")
        .send()
        .await?
        .text()
        .await?;
    assert!(frag.contains("class=\"view-toggle\""), "{frag}");
    Ok(())
}

/// Gallery cards expose hover quick actions: force-refresh and open-origin.
#[tokio::test]
async fn gallery_card_has_quick_actions() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    fs::create_dir_all(&repo)?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main","origin":"https://github.com/owner/demo.git"}"#,
    )?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();
    let html = client.get(format!("{base}/")).send().await?.text().await?;

    assert!(html.contains("card-actions"), "card actions missing: {html}");
    assert!(html.contains(r#"hx-post="/repos/owner-demo/refresh""#), "{html}");
    assert!(html.contains(r#"href="https://github.com/owner/demo.git""#), "{html}");

    // unidentified repos (no origin) only get the refresh action
    let mystery = root.join("_unknown/mystery");
    fs::create_dir_all(&mystery)?;
    fs::write(
        mystery.join("repo.json"),
        r#"{"forge":"generic","name":"mystery","added":"2025-01-01T00:00:00Z","default_branch":"main","unidentified":true}"#,
    )?;
    let (base2, _st2) = spawn_server(test_cfg(&root)).await;
    let page = reqwest::get(format!("{base2}/")).await?.text().await?;
    assert!(page.contains(r#"hx-post="/repos/_unknown/mystery/refresh""#), "{page}");
    assert_eq!(
        page.matches(r#"title="Open origin""#).count(),
        1,
        "only the repo with an origin gets the action: {page}"
    );
    Ok(())
}

/// User-chosen color labels round-trip through the manifest and render as a
/// colored dot badge on the card and detail header.
#[tokio::test]
async fn metadata_color_badge_roundtrips() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    fs::create_dir_all(&repo)?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/repos/owner-demo/metadata"))
        .form(&[("name", "demo"), ("description", ""), ("notes", ""), ("color", "blue")])
        .send()
        .await?;
    assert_eq!(resp.status(), 200);

    let m: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("repo.json"))?)?;
    assert_eq!(m["color"], "blue");

    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("card tinted"), "card should be tinted: {page}");
    assert!(page.contains("data-color=\"blue\""), "card should carry the color: {page}");
    let detail = client.get(format!("{base}/repos/owner-demo")).send().await?.text().await?;
    assert!(detail.contains("panel tinted"), "About panel should be tinted: {detail}");
    assert!(detail.contains("data-color=\"blue\""), "About panel should carry the color: {detail}");
    assert!(detail.contains("name=\"color\""), "metadata should offer color swatches: {detail}");

    // clearing the color removes the badge
    client
        .post(format!("{base}/repos/owner-demo/metadata"))
        .form(&[("name", "demo"), ("description", ""), ("notes", ""), ("color", "")])
        .send()
        .await?;
    let m2: serde_json::Value = serde_json::from_str(&fs::read_to_string(repo.join("repo.json"))?)?;
    assert!(m2["color"].is_null(), "empty color should clear the badge: {m2}");
    Ok(())
}

/// A stored owner avatar is served from the archive and shown on the card and
/// detail header. Repos without one 404 and show no icon.
#[tokio::test]
async fn repo_icon_is_stored_and_served() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let repo = root.join("owner-demo");
    let plain = root.join("owner-plain");
    let empty = root.join("owner-empty");
    fs::create_dir_all(&repo)?;
    fs::create_dir_all(&plain)?;
    fs::create_dir_all(&empty)?;
    fs::write(
        repo.join("repo.json"),
        r#"{"forge":"github","name":"demo","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    fs::write(
        plain.join("repo.json"),
        r#"{"forge":"github","name":"plain","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    fs::write(
        empty.join("repo.json"),
        r#"{"forge":"github","name":"empty","added":"2025-01-01T00:00:00Z","default_branch":"main"}"#,
    )?;
    let icon: &[u8] = b"\x89PNG\r\n\x1a\nfake-avatar-bytes";
    fs::write(repo.join("icon"), icon)?;
    // zero-byte marker = "checked, no avatar" -> not an icon
    fs::write(empty.join("icon"), b"")?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let resp = client.get(format!("{base}/repo-icon/owner-demo")).send().await?;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap().to_str().unwrap(), "image/png");
    assert_eq!(resp.bytes().await?.as_ref(), icon);

    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains(r#"src="/repo-icon/owner-demo""#), "card icon missing: {page}");
    assert!(!page.contains(r#"src="/repo-icon/owner-plain""#), "plain repo should have no icon: {page}");
    assert!(!page.contains(r#"src="/repo-icon/owner-empty""#), "zero-byte marker is not an icon: {page}");

    let detail = client.get(format!("{base}/repos/owner-demo")).send().await?.text().await?;
    assert!(detail.contains("repo-icon-lg"), "detail icon missing: {detail}");
    assert!(detail.contains(r#"src="/repo-icon/owner-demo""#), "{detail}");

    assert_eq!(
        client.get(format!("{base}/repo-icon/owner-plain")).send().await?.status(),
        404
    );
    // a zero-byte marker file must not be served either
    assert_eq!(
        client.get(format!("{base}/repo-icon/owner-empty")).send().await?.status(),
        404
    );
    Ok(())
}

fn write_min_repo(root: &Path, rel: &str, name: &str) {
    let dir = root.join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("repo.json"),
        format!(r#"{{"forge":"generic","name":"{name}","added":"2025-01-01T00:00:00Z","default_branch":"main"}}"#),
    )
    .unwrap();
}

fn read_tags(root: &Path, rel: &str) -> Vec<String> {
    let raw = fs::read_to_string(root.join(rel).join("repo.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    v["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str().map(str::to_string)).collect())
        .unwrap_or_default()
}

/// Bulk tag, move and delete across several repos, plus the selection UI.
#[tokio::test]
async fn bulk_operations_tag_move_and_delete() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    write_min_repo(&root, "owner-a", "a");
    write_min_repo(&root, "owner-b", "b");
    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    // The grid renders the selection controls.
    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(page.contains("id=\"bulk-bar\""), "bulk bar missing: {page}");
    assert!(page.contains("class=\"bulk-check\""), "selection checkboxes missing");
    assert!(page.contains("hx-post=\"/repos/bulk\""), "bulk action buttons missing");

    let rels = serde_json::json!(["owner-a", "owner-b"]).to_string();

    // Add a tag to both.
    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "tag_add"), ("repos", rels.as_str()), ("tag", "bulk")])
        .send()
        .await?;
    assert!(resp.status().is_success());
    let body = resp.text().await?;
    assert!(body.contains("added to 2 repositories"), "{body}");
    assert_eq!(read_tags(&root, "owner-a"), vec!["bulk"]);
    assert_eq!(read_tags(&root, "owner-b"), vec!["bulk"]);

    // Remove it again.
    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "tag_remove"), ("repos", rels.as_str()), ("tag", "bulk")])
        .send()
        .await?;
    let body = resp.text().await?;
    assert!(body.contains("removed from 2 repositories"), "{body}");
    assert!(read_tags(&root, "owner-a").is_empty());

    // Move both into a folder.
    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "move"), ("repos", rels.as_str()), ("folder", "cat")])
        .send()
        .await?;
    let body = resp.text().await?;
    assert!(body.contains("Moved 2 repositories"), "{body}");
    assert!(root.join("cat/owner-a/repo.json").exists(), "owner-a not moved");
    assert!(root.join("cat/owner-b/repo.json").exists(), "owner-b not moved");

    // Delete one from the index but keep its files.
    let one = serde_json::json!(["cat/owner-a"]).to_string();
    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "delete"), ("repos", one.as_str())])
        .send()
        .await?;
    let body = resp.text().await?;
    assert!(body.contains("Removed 1 repositories"), "{body}");
    assert!(!root.join("cat/owner-a/repo.json").exists(), "manifest should be gone");
    assert!(root.join("cat/owner-a").exists(), "files should be kept by default");
    assert!(root.join("cat/owner-b/repo.json").exists(), "owner-b should be untouched");
    Ok(())
}

/// An empty selection and an unknown action are rejected without touching disk.
#[tokio::test]
async fn bulk_rejects_empty_selection_and_bad_action() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    write_min_repo(&root, "owner-a", "a");
    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "tag_add"), ("repos", "[]"), ("tag", "x")])
        .send()
        .await?;
    assert!(resp.text().await?.contains("Select at least one repository"));

    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "explode"), ("repos", "[\"owner-a\"]")])
        .send()
        .await?;
    assert!(resp.text().await?.contains("Unknown bulk action"));

    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "tag_add"), ("repos", "[\"owner-a\"]"), ("tag", "  ")])
        .send()
        .await?;
    assert!(resp.text().await?.contains("Enter a tag first"));
    Ok(())
}

fn write_repo_with_origin(root: &Path, rel: &str, name: &str, origin: &str) {
    let dir = root.join(rel);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("repo.json"),
        format!(
            r#"{{"forge":"generic","name":"{name}","added":"2025-01-01T00:00:00Z","default_branch":"master","origin":"{origin}"}}"#
        ),
    )
    .unwrap();
}

/// Bulk refresh must actually queue the jobs and archive each repo.
#[tokio::test]
async fn bulk_refresh_archives_every_selected_repo() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    let remote = make_remote(tmp.path(), "bulkproj");
    let origin = format!("file://{}", remote.display());
    write_repo_with_origin(&root, "owner-a", "a", &origin);
    write_repo_with_origin(&root, "owner-b", "b", &origin);
    // One permit for two repos exercises the shared refresh semaphore.
    let mut cfg = test_cfg(&root);
    cfg.scheduler.max_concurrent = 1;
    let (base, _st) = spawn_server(cfg).await;
    let client = reqwest::Client::new();

    let rels = serde_json::json!(["owner-a", "owner-b"]).to_string();
    let resp = client
        .post(format!("{base}/repos/bulk"))
        .form(&[("action", "refresh"), ("repos", rels.as_str())])
        .send()
        .await?;
    let body = resp.text().await?;
    assert!(body.contains("Refresh queued for 2 repositories"), "{body}");

    // Wait for the (real) clones to produce snapshots.
    for _ in 0..200 {
        if root.join("owner-a/branch/master").is_dir() && root.join("owner-b/branch/master").is_dir() {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("bulk refresh did not archive both repos");
}

/// The bulk bar is buttons only; tag and move now live in modals.
#[tokio::test]
async fn bulk_modals_render() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    write_min_repo(&root, "owner-a", "a");
    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let tag = client.get(format!("{base}/repos/bulk/tag")).send().await?.text().await?;
    assert!(tag.contains("id=\"bulk-modal\""), "tag modal missing: {tag}");
    assert!(tag.contains("id=\"bulk-tag-input\""));
    assert!(tag.contains("tag_add") && tag.contains("tag_remove"));

    let mv = client.get(format!("{base}/repos/bulk/move")).send().await?.text().await?;
    assert!(mv.contains("id=\"bulk-modal\""), "move modal missing: {mv}");
    assert!(mv.contains("id=\"bulk-move-folder\""));
    assert!(mv.contains("action:\"move\""));

    let page = client.get(format!("{base}/")).send().await?.text().await?;
    assert!(!page.contains("id=\"bulk-tag\""), "bar should not carry an inline tag input");
    assert!(!page.contains("id=\"bulk-folder\""), "bar should not carry an inline folder input");
    assert!(page.contains("hx-get=\"/repos/bulk/tag\""), "Tag button missing");
    assert!(page.contains("hx-get=\"/repos/bulk/move\""), "Move button missing");
    Ok(())
}

/// Storage totals and rate-limit status must appear in /api/stats and on /stats.
#[tokio::test]
async fn stats_report_storage_and_rate_limits() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path().join("archive");
    write_min_repo(&root, "owner-demo", "demo");
    let branch = root.join("owner-demo/branch/main");
    fs::create_dir_all(&branch)?;
    fs::write(
        branch.join("demo-main@2025-01-01_abc.json"),
        r#"{"kind":"branch-snapshot","repo":"owner-demo","origin":"file:///r/demo","ref":"main","commit":"abc","archived_at":"2025-01-01T00:00:00Z","format":"zip","zip":{"file":"demo-main@2025-01-01_abc.zip","bytes":2048,"sha256":"a"}}"#,
    )?;

    let (base, _st) = spawn_server(test_cfg(&root)).await;
    let client = reqwest::Client::new();

    let stats: serde_json::Value =
        client.get(format!("{base}/api/stats")).send().await?.json().await?;
    assert_eq!(stats["totals"]["archive_bytes"], 2048, "{stats}");
    assert_eq!(stats["totals"]["snapshot_bytes"], 2048, "{stats}");
    assert_eq!(stats["totals"]["largest_repos"][0]["rel"], "owner-demo");
    assert!(stats["totals"]["rate_limited"].is_number(), "{stats}");
    assert!(stats["totals"]["requests_skipped"].is_number(), "{stats}");
    assert!(stats["totals"]["paused_hosts"].is_array(), "{stats}");

    let page = client.get(format!("{base}/stats")).send().await?.text().await?;
    assert!(page.contains("Storage"), "storage section missing");
    assert!(page.contains("Largest repositories"), "largest repos table missing");
    assert!(page.contains("Remote rate limits"), "rate-limit section missing");
    assert!(page.contains("No hosts are paused right now."));
    Ok(())
}
