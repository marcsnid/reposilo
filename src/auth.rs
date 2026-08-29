//! User accounts + session auth. Auth is opt-in: with no
//! `users.json` in the archive root, everything stays open (backwards
//! compatible). Passwords are argon2-hashed; sessions are in-memory with a
//! one-week cookie. users.json is re-read when its mtime changes so the CLI
//! can manage users while the server runs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use axum::body::Body;
use axum::extract::{Form, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::IntoResponse;
use askama::Template as _;
pub use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::server::AppState;

const SESSION_COOKIE: &str = "rs_session";
const SESSION_SECS: u64 = 7 * 24 * 3600;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub username: String,
    pub password_hash: String, // argon2 encoded (includes salt + params)
    pub created: String,
    #[serde(default)]
    pub last_seen: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct UsersFile {
    users: Vec<User>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub username: String,
}

// ---------- users.json management (archive-root file, reloaded on mtime) ----------

#[derive(Default)]
pub struct Users {
    users: Vec<User>,
    path: Option<PathBuf>,
    mtime: Option<std::time::SystemTime>,
}

impl Users {
    pub fn load(root: &Path) -> Self {
        let path = root.join("users.json");
        let mut u = Users { path: Some(path.clone()), ..Default::default() };
        u.reload();
        u
    }

    fn reload(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(meta) = std::fs::metadata(path) else {
            self.users.clear();
            self.mtime = None;
            return;
        };
        if Some(meta.modified().unwrap_or(std::time::UNIX_EPOCH)) == self.mtime && !self.users.is_empty() {
            return;
        }
        match std::fs::read_to_string(path) {
            Ok(s) => match serde_json::from_str::<UsersFile>(&s) {
                Ok(f) => {
                    self.users = f.users;
                    self.mtime = meta.modified().ok();
                }
                Err(_) => self.users.clear(),
            },
            Err(_) => self.users.clear(),
        }
    }

    pub fn is_empty(&mut self) -> bool {
        self.reload();
        self.users.is_empty()
    }

    pub fn verify(&mut self, username: &str, password: &str) -> Option<User> {
        self.reload();
        let user = self.users.iter().find(|u| u.username == username)?.clone();
        argon2::verify_encoded(&user.password_hash, password.as_bytes())
            .ok()
            .filter(|ok| *ok)
            .map(|_| user)
    }

    pub fn find(&mut self, username: &str) -> Option<User> {
        self.reload();
        self.users.iter().find(|u| u.username == username).cloned()
    }
}

// ---------- CLI-facing helpers ----------

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = random_hex()?;
    Ok(argon2::hash_encoded(password.as_bytes(), salt.as_bytes(), &argon2::Config::default())?)
}

pub fn users_path(root: &Path) -> PathBuf {
    root.join("users.json")
}

pub fn add_user(root: &Path, username: &str, password: &str) -> anyhow::Result<()> {
    let mut file: UsersFile = match std::fs::read_to_string(users_path(root)) {
        Ok(s) => serde_json::from_str(&s)?,
        Err(_) => UsersFile::default(),
    };
    if file.users.iter().any(|u| u.username == username) {
        anyhow::bail!("user {username} already exists");
    }
    file.users.push(User {
        username: username.to_string(),
        password_hash: hash_password(password)?,
        created: crate::archiver::now_rfc3339(),
        last_seen: None,
    });
    std::fs::write(users_path(root), serde_json::to_string_pretty(&file)?)?;
    Ok(())
}

pub fn remove_user(root: &Path, username: &str) -> anyhow::Result<()> {
    let s = std::fs::read_to_string(users_path(root))?;
    let mut file: UsersFile = serde_json::from_str(&s)?;
    let before = file.users.len();
    file.users.retain(|u| u.username != username);
    if file.users.len() == before {
        anyhow::bail!("no such user: {username}");
    }
    std::fs::write(users_path(root), serde_json::to_string_pretty(&file)?)?;
    Ok(())
}

pub fn list_users(root: &Path) -> anyhow::Result<Vec<String>> {
    let s = std::fs::read_to_string(users_path(root))?;
    let file: UsersFile = serde_json::from_str(&s)?;
    Ok(file.users.into_iter().map(|u| u.username).collect())
}

pub fn set_last_seen(root: &Path, username: &str) -> anyhow::Result<()> {
    let s = std::fs::read_to_string(users_path(root))?;
    let mut file: UsersFile = serde_json::from_str(&s)?;
    if let Some(u) = file.users.iter_mut().find(|u| u.username == username) {
        u.last_seen = Some(crate::archiver::now_rfc3339());
    }
    std::fs::write(users_path(root), serde_json::to_string_pretty(&file)?)?;
    Ok(())
}

fn random_hex() -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        f.read_exact(&mut buf)?;
    } else {
        // fallback: hash of time + counter (not unix, or urandom unavailable)
        use sha2::{Digest, Sha256};
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let mut h = Sha256::new();
        h.update(t.as_nanos().to_le_bytes());
        h.update(t.subsec_nanos().to_le_bytes());
        buf.copy_from_slice(&h.finalize()[..32]);
    }
    Ok(hex::encode(buf))
}

// ---------- sessions (in-memory) ----------

pub struct SessionStore {
    sessions: std::sync::Mutex<HashMap<String, Session>>,
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    pub fn new() -> Self {
        Self { sessions: Default::default() }
    }

    pub fn create(&self, username: &str) -> String {
        let token = random_hex().unwrap_or_else(|_| hex::encode(crate::archiver::now_rfc3339()));
        self.sessions
            .lock()
            .unwrap()
            .insert(token.clone(), Session { username: username.to_string() });
        token
    }

    pub fn get(&self, token: &str) -> Option<Session> {
        self.sessions.lock().unwrap().get(token).cloned()
    }

    pub fn remove(&self, token: &str) {
        self.sessions.lock().unwrap().remove(token);
    }
}

// ---------- handlers ----------

fn session_token_from_headers(headers: &HeaderMap) -> Option<String> {
    for (k, v) in headers.iter() {
        if k == header::COOKIE {
            let s = v.to_str().ok()?;
            for part in s.split(';') {
                let part = part.trim();
                if let Some(t) = part.strip_prefix(&format!("{SESSION_COOKIE}=")) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

/// Current logged-in username (None when auth is off).
pub fn current_user(st: &Arc<AppState>, headers: &HeaderMap) -> Option<String> {
    let users_exist = st.users.lock().map(|mut u| !u.is_empty()).unwrap_or(false);
    if !users_exist {
        return None; // auth disabled
    }
    let token = session_token_from_headers(headers)?;
    st.sessions.get(&token).map(|s| s.username)
}

/// GET /login: renders the login page template (same base → same icon/styles)
pub async fn login_page(st: State<Arc<AppState>>) -> Response {
    if st.users.lock().map(|mut u| u.is_empty()).unwrap_or(true) {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, "/")],
        )
            .into_response();
    }
    let ctx = crate::web::views::LoginCtx { error: String::new() };
    match (crate::web::views::LoginT { ctx }).render() {
        Ok(html) => (
            [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
            Body::from(html),
        )
            .into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}")).into_response(),
    }
}

/// POST /login (form): success sets a cookie + redirects; failure renders
/// the same page with a styled error banner
pub async fn login_submit(st: State<Arc<AppState>>, Form(form): Form<HashMap<String, String>>) -> Response {
    let username = form.get("username").cloned().unwrap_or_default();
    let password = form.get("password").cloned().unwrap_or_default();
    let user = st
        .users
        .lock()
        .ok()
        .and_then(|mut u| u.verify(&username, &password));
    match user {
        Some(user) => {
            let _ = set_last_seen(&st.root().await, &user.username);
            let token = st.sessions.create(&user.username);
            let mut resp = Response::builder()
                .status(StatusCode::SEE_OTHER)
                .header(header::LOCATION, "/")
                .body(Body::empty())
                .unwrap();
            resp.headers_mut().insert(
                header::SET_COOKIE,
                axum::http::HeaderValue::from_str(&format!(
                    "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_SECS}"
                ))
                .unwrap(),
            );
            resp
        }
        None => {
            let ctx = crate::web::views::LoginCtx { error: "wrong username or password".into() };
            match (crate::web::views::LoginT { ctx }).render() {
                Ok(html) => (
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    Body::from(html),
                )
                    .into_response(),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("template error: {e}")).into_response(),
            }
        }
    }
}

/// POST /logout: clears the session and redirects to /login.
pub async fn logout(st: State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_headers(&headers) {
        st.sessions.remove(&token);
    }
    let mut resp = Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/login")
        .body(Body::empty())
        .unwrap();
    resp.headers_mut().insert(
        header::SET_COOKIE,
        axum::http::HeaderValue::from_str(&format!("{SESSION_COOKIE}=; Path=/; HttpOnly; Max-Age=0")).unwrap(),
    );
    resp
}

// ---------- middleware ----------

fn is_open_path(path: &str) -> bool {
    path == "/login" || path.starts_with("/assets/") || path == "/logout"
}

pub async fn mw(st: State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_open_path(&path) {
        return next.run(req).await;
    }
    let users_exist = st.users.lock().ok().map(|mut u| !u.is_empty()).unwrap_or(false);
    if !users_exist {
        return next.run(req).await; // auth disabled: no users.json
    }
    let token = session_token_from_headers(req.headers());
    if token.as_deref().and_then(|t| st.sessions.get(t)).is_some() {
        return next.run(req).await;
    }
    // not logged in
    let is_api = path.starts_with("/api/");
    let is_hx = req.headers().get("hx-request").is_some();
    if is_api {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"error":"login required"}"#))
            .unwrap();
    }
    if is_hx {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header("hx-redirect", "/login")
            .body(Body::empty())
            .unwrap();
    }
    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, "/login")],
    )
        .into_response()
}