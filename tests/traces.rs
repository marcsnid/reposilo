//! End-to-end OTLP traces: with `[otel] traces` on, a recorded span must be
//! exported to a collector over HTTP. The collector here is a minimal local
//! server that captures request bodies.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reposilo::config::Config;
use reposilo::telemetry;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// A mock OTLP/HTTP collector. Any POST body is captured and answered 200.
async fn spawn_collector() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies2 = bodies.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let mut data: Vec<u8> = Vec::new();
                let mut buf = [0u8; 8192];
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    data.extend_from_slice(&buf[..n]);
                    if let Some(pos) = find(&data, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if data.len() >= pos + 4 + len {
                            break;
                        }
                    }
                }
                if let Some(pos) = find(&data, b"\r\n\r\n") {
                    bodies2
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&data[pos + 4..]).to_string());
                }
                let _ = sock
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await;
                let _ = sock.flush().await;
            }
        });
    });
    (format!("http://{addr}"), bodies)
}

#[test]
fn spans_are_exported_to_the_collector() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (endpoint, bodies) = rt.block_on(spawn_collector());

    let mut cfg = Config::default();
    cfg.otel.enabled = true;
    cfg.otel.endpoint = endpoint;
    cfg.otel.logs = false;
    cfg.otel.metrics = false;
    cfg.otel.traces = true;
    cfg.otel.service_name = "reposilo-traces-test".into();

    let telemetry = telemetry::init(&cfg);
    assert!(telemetry.traces_enabled(), "traces should be enabled");
    telemetry::install_subscriber(&telemetry);

    {
        let span = tracing::info_span!("reposilo.test.span", answer = 42);
        let _entered = span.enter();
        tracing::info!("inside the span");
    }
    // Flush the batch exporter.
    telemetry.shutdown();

    for _ in 0..50 {
        if !bodies.lock().unwrap().is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let captured = bodies.lock().unwrap().join("\n");
    assert!(
        captured.contains("reposilo.test.span"),
        "span name was not exported to the collector ({} byte body)",
        captured.len()
    );
}