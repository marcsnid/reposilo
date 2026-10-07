//! Minimal local HTTP server for unit tests. Compiled only under `cfg(test)`.
//!
//! It answers every request with a fixed status and body, and records the
//! request line and headers so a test can assert the URL and auth that were
//! actually sent. It runs on its own thread and runtime, so the test future
//! can never starve it.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    /// The request line, e.g. `GET /repos/o/r HTTP/1.1`.
    pub line: String,
    /// Everything after the request line (headers).
    pub headers: String,
}

impl RecordedRequest {
    /// The request path, without the query string.
    pub fn path(&self) -> String {
        self.line
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string()
    }
}

pub struct MockServer {
    pub base: String,
    pub requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockServer {
    pub fn paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(RecordedRequest::path).collect()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

/// Spawn a server that answers every request with `status` and `body`.
pub async fn spawn(status: u16, body: &'static str) -> MockServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let _ = sock.set_nodelay(true);
                let mut buf = [0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                let (line, headers) = text
                    .split_once("\r\n")
                    .map(|(l, h)| (l.to_string(), h.to_string()))
                    .unwrap_or((text, String::new()));
                recorded.lock().unwrap().push(RecordedRequest { line, headers });
                let head = format!(
                    "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(body.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
    });
    MockServer { base: format!("http://{addr}"), requests }
}