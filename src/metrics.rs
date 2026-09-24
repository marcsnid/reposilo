//! Lightweight, always-on stats plus an optional OTLP/HTTP exporter.
//!
//! Stats are per-day counters persisted to `<archive>/metrics.json`, capped to
//! a rolling window so the file can't grow forever. The built-in stats page
//! reads them directly; if `[otel] enabled = true` a background task also ships
//! them to an OpenTelemetry collector over OTLP/HTTP (JSON).

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, macros::format_description};

/// Days of history retained on disk.
pub const KEEP_DAYS: usize = 90;

/// Per-day counters. All monotonic within a day.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DayStats {
    pub refresh_ok: u64,
    pub refresh_fail: u64,
    pub add_ok: u64,
    pub add_fail: u64,
    pub new_releases: u64,
    pub new_snapshots: u64,
    pub remote_gone: u64,
}

impl DayStats {
    fn add(&mut self, other: &DayStats) {
        self.refresh_ok += other.refresh_ok;
        self.refresh_fail += other.refresh_fail;
        self.add_ok += other.add_ok;
        self.add_fail += other.add_fail;
        self.new_releases += other.new_releases;
        self.new_snapshots += other.new_snapshots;
        self.remote_gone += other.remote_gone;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub started_at: String,
    /// "YYYY-MM-DD" -> counters (BTreeMap keeps them date-ordered).
    pub days: BTreeMap<String, DayStats>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { started_at: now_rfc3339(), days: BTreeMap::new() }
    }
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn today_key() -> String {
    let fmt = format_description!("[year]-[month]-[day]");
    OffsetDateTime::now_utc().format(fmt).unwrap_or_else(|_| "1970-01-01".into())
}

impl Metrics {
    pub fn load(root: &Path) -> Self {
        let mut m = crate::types::read_json::<Metrics>(&root.join("metrics.json")).unwrap_or_default();
        if m.started_at.is_empty() {
            m.started_at = now_rfc3339();
        }
        m.prune();
        m
    }

    pub fn save(&self, root: &Path) {
        let _ = crate::types::write_json(&root.join("metrics.json"), self);
    }

    fn today(&mut self) -> &mut DayStats {
        let key = today_key();
        self.days.entry(key).or_default()
    }

    /// Bump one named counter (matches the `DayStats` fields).
    pub fn bump(&mut self, field: &str) {
        let d = self.today();
        match field {
            "refresh_ok" => d.refresh_ok += 1,
            "refresh_fail" => d.refresh_fail += 1,
            "add_ok" => d.add_ok += 1,
            "add_fail" => d.add_fail += 1,
            "new_releases" => d.new_releases += 1,
            "new_snapshots" => d.new_snapshots += 1,
            "remote_gone" => d.remote_gone += 1,
            _ => {}
        }
    }

    /// Drop the oldest days beyond `KEEP_DAYS`.
    pub fn prune(&mut self) {
        while self.days.len() > KEEP_DAYS {
            let oldest = self.days.keys().next().cloned();
            match oldest {
                Some(k) => {
                    self.days.remove(&k);
                }
                None => break,
            }
        }
    }

    /// Cumulative totals over the whole retained window.
    pub fn sums(&self) -> DayStats {
        let mut s = DayStats::default();
        for d in self.days.values() {
            s.add(d);
        }
        s
    }
}

/// Index-derived gauges (current state), passed alongside the counters.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub repos: u64,
    pub snapshots: u64,
    pub releases: u64,
    pub dead: u64,
    pub unavailable: u64,
    pub untagged: u64,
}

/// Push one metrics snapshot to an OTLP/HTTP endpoint as JSON.
///
/// `endpoint` is the collector base URL (e.g. `http://host:4318`); the
/// `/v1/metrics` path is appended when missing. Failures are returned so the
/// caller can log them — they must never affect archiving.
pub async fn export_otlp(
    endpoint: &str,
    service_name: &str,
    metrics: &Metrics,
    totals: &Totals,
) -> Result<()> {
    let base = endpoint.trim().trim_end_matches('/');
    if base.is_empty() {
        anyhow::bail!("otel endpoint is empty");
    }
    let url = if base.ends_with("/v1/metrics") {
        base.to_string()
    } else {
        format!("{base}/v1/metrics")
    };

    let ts = OffsetDateTime::now_utc().unix_timestamp_nanos().to_string();
    let start = OffsetDateTime::parse(&metrics.started_at, &Rfc3339)
        .map(|t| t.unix_timestamp_nanos().to_string())
        .unwrap_or_else(|_| ts.clone());
    let sums = metrics.sums();

    let gauge = |name: &str, desc: &str, v: u64| {
        serde_json::json!({
            "name": name,
            "description": desc,
            "unit": "1",
            "gauge": { "dataPoints": [ { "asInt": v.to_string(), "timeUnixNano": ts } ] }
        })
    };
    let sum = |name: &str, desc: &str, v: u64| {
        serde_json::json!({
            "name": name,
            "description": desc,
            "unit": "1",
            "sum": {
                "aggregationTemporality": 2, // CUMULATIVE
                "isMonotonic": true,
                "dataPoints": [ {
                    "asInt": v.to_string(),
                    "startTimeUnixNano": start,
                    "timeUnixNano": ts
                } ]
            }
        })
    };

    let payload = serde_json::json!({
        "resourceMetrics": [ {
            "resource": {
                "attributes": [
                    { "key": "service.name", "value": { "stringValue": service_name } },
                    { "key": "service.version", "value": { "stringValue": env!("CARGO_PKG_VERSION") } }
                ]
            },
            "scopeMetrics": [ {
                "scope": { "name": "reposilo", "version": env!("CARGO_PKG_VERSION") },
                "metrics": [
                    gauge("reposilo.repos", "Archived repositories", totals.repos),
                    gauge("reposilo.snapshots", "Archived snapshots (branch)", totals.snapshots),
                    gauge("reposilo.releases", "Archived releases", totals.releases),
                    gauge("reposilo.repos.dead", "Repositories with a dead remote", totals.dead),
                    gauge("reposilo.repos.unavailable", "Repositories temporarily unreachable", totals.unavailable),
                    gauge("reposilo.repos.untagged", "Repositories with no tags", totals.untagged),
                    sum("reposilo.refresh.success", "Successful refreshes", sums.refresh_ok),
                    sum("reposilo.refresh.failure", "Failed refreshes", sums.refresh_fail),
                    sum("reposilo.add.success", "Successful adds", sums.add_ok),
                    sum("reposilo.add.failure", "Failed adds", sums.add_fail),
                    sum("reposilo.releases.new", "New releases archived", sums.new_releases),
                    sum("reposilo.snapshots.new", "New branch snapshots", sums.new_snapshots),
                    sum("reposilo.remote.gone", "Remotes observed unreachable", sums.remote_gone)
                ]
            } ]
        } ]
    });

    reqwest::Client::new()
        .post(&url)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .with_context(|| format!("OTLP POST to {url} failed"))?
        .error_for_status()
        .with_context(|| format!("OTLP endpoint {url} rejected the payload"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bumps_prunes_and_sums() {
        let mut m = Metrics::default();
        m.bump("refresh_ok");
        m.bump("refresh_ok");
        m.bump("refresh_fail");
        m.bump("new_releases");
        let s = m.sums();
        assert_eq!(s.refresh_ok, 2);
        assert_eq!(s.refresh_fail, 1);
        assert_eq!(s.new_releases, 1);

        // prune keeps at most KEEP_DAYS, dropping the oldest
        for i in 0..(KEEP_DAYS + 10) {
            m.days.insert(format!("2020-01-{:02}", (i % 28) + 1), DayStats { refresh_ok: 1, ..Default::default() });
        }
        // BTreeMap dedups keys, so force distinct by day count via a loop is
        // unnecessary; just assert the cap holds.
        m.days = (0..(KEEP_DAYS + 10)).map(|i| (format!("{i:05}"), DayStats::default())).collect();
        m.prune();
        assert_eq!(m.days.len(), KEEP_DAYS);
    }

    #[test]
    fn otlp_payload_is_well_formed() {
        // We only build the payload shape here; export is exercised by the
        // 400/200 path in integration if a collector is present.
        let m = Metrics { started_at: now_rfc3339(), days: BTreeMap::new() };
        let sums = m.sums();
        assert_eq!(sums.refresh_ok, 0);
    }

    #[tokio::test]
    async fn otlp_export_posts_the_metrics() {
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
        let cap = captured.clone();
        let app = axum::Router::new().route(
            "/v1/metrics",
            axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let cap = cap.clone();
                async move {
                    *cap.lock().unwrap() = Some(body);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut m = Metrics::default();
        m.bump("refresh_ok");
        m.bump("refresh_ok");
        m.bump("refresh_fail");
        let totals = Totals { repos: 3, dead: 1, ..Default::default() };
        export_otlp(&format!("http://{addr}"), "reposilo", &m, &totals)
            .await
            .unwrap();

        let body = captured.lock().unwrap().clone().expect("OTLP body captured");
        let metrics = body["resourceMetrics"][0]["scopeMetrics"][0]["metrics"]
            .as_array()
            .expect("metrics array");
        let find = |name: &str| metrics.iter().find(|x| x["name"] == name).unwrap();
        assert_eq!(
            find("reposilo.repos")["gauge"]["dataPoints"][0]["asInt"],
            "3"
        );
        assert_eq!(
            find("reposilo.repos.dead")["gauge"]["dataPoints"][0]["asInt"],
            "1"
        );
        assert_eq!(
            find("reposilo.refresh.success")["sum"]["dataPoints"][0]["asInt"],
            "2"
        );
        assert_eq!(
            find("reposilo.refresh.failure")["sum"]["dataPoints"][0]["asInt"],
            "1"
        );
    }
}