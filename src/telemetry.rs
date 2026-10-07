//! Optional OpenTelemetry export: logs bridged from `tracing`, and metrics.
//!
//! Providers are built once after config load, the logging bridge is installed
//! into the global tracing subscriber, and the handles are handed to the rest
//! of the program (`AppState`) so they stay alive and can be flushed on exit.

use std::time::Duration;

use anyhow::{Context, Result};
use opentelemetry::metrics::{Counter, Histogram, Meter, MeterProvider};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::Resource;

use crate::config::Config;
use crate::metrics::Totals;

/// Event counters, incremented where the event happens rather than read from
/// the persisted per-day store. The SDK exports a cumulative sum, so a TSDB
/// can `rate()`/`increase()` over any window (a process restart is just a
/// counter reset, which backends handle).
#[derive(Clone)]
struct Counters {
    refresh_ok: Counter<u64>,
    refresh_fail: Counter<u64>,
    add_ok: Counter<u64>,
    add_fail: Counter<u64>,
    new_releases: Counter<u64>,
    new_snapshots: Counter<u64>,
    remote_gone: Counter<u64>,
    verify_runs: Counter<u64>,
    verify_problems: Counter<u64>,
    verify_fail: Counter<u64>,
    remote_rate_limited: Counter<u64>,
    remote_requests_skipped: Counter<u64>,
}

impl Counters {
    fn new(meter: &Meter) -> Self {
        let counter = |name: &'static str, desc: &'static str| {
            meter.u64_counter(name).with_description(desc).build()
        };
        Self {
            refresh_ok: counter("reposilo.refresh.success", "Successful refreshes"),
            refresh_fail: counter("reposilo.refresh.failure", "Failed refreshes"),
            add_ok: counter("reposilo.add.success", "Successful adds"),
            add_fail: counter("reposilo.add.failure", "Failed adds"),
            new_releases: counter("reposilo.releases.new", "New releases archived"),
            new_snapshots: counter("reposilo.snapshots.new", "New branch snapshots"),
            remote_gone: counter("reposilo.remote.gone", "Remotes observed unreachable"),
            verify_runs: counter("reposilo.verify.runs", "Integrity checks completed"),
            verify_problems: counter("reposilo.verify.problems", "Integrity problems detected"),
            verify_fail: counter("reposilo.verify.failure", "Integrity checks that failed to run"),
            remote_rate_limited: counter(
                "reposilo.remote.rate_limited",
                "Hosts paused by a rate-limit signal",
            ),
            remote_requests_skipped: counter(
                "reposilo.remote.requests_skipped",
                "Requests skipped because a host was rate-limited",
            ),
        }
    }

    fn bump(&self, field: &str) {
        self.bump_count(field, 1);
    }

    fn bump_count(&self, field: &str, n: u64) {
        match field {
            "refresh_ok" => self.refresh_ok.add(n, &[]),
            "refresh_fail" => self.refresh_fail.add(n, &[]),
            "add_ok" => self.add_ok.add(n, &[]),
            "add_fail" => self.add_fail.add(n, &[]),
            "new_releases" => self.new_releases.add(n, &[]),
            "new_snapshots" => self.new_snapshots.add(n, &[]),
            "remote_gone" => self.remote_gone.add(n, &[]),
            "verify_runs" => self.verify_runs.add(n, &[]),
            "verify_problems" => self.verify_problems.add(n, &[]),
            "verify_fail" => self.verify_fail.add(n, &[]),
            _ => {}
        }
    }
}

/// Live OpenTelemetry providers (or nothing when `[otel] enabled = false`).
#[derive(Clone)]
pub struct Telemetry {
    logger: Option<SdkLoggerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    meter: Option<Meter>,
    counters: Option<Counters>,
    verify_duration: Option<Histogram<f64>>,
}

impl Telemetry {
    pub fn disabled() -> Self {
        Self {
            logger: None,
            meter_provider: None,
            meter: None,
            counters: None,
            verify_duration: None,
        }
    }

    pub fn logger(&self) -> Option<&SdkLoggerProvider> {
        self.logger.as_ref()
    }

    pub fn enabled(&self) -> bool {
        self.logger.is_some() || self.meter.is_some()
    }

    /// Flush and close the providers. Idempotent.
    pub fn shutdown(&self) {
        if let Some(p) = &self.logger {
            let _ = p.shutdown();
        }
        if let Some(p) = &self.meter_provider {
            let _ = p.shutdown();
        }
    }

    /// Bump the counters for the events a job reported (same field names the
    /// persisted stats use). Called from `AppState::record`.
    pub fn record_events(&self, fields: &[&str]) {
        let Some(counters) = &self.counters else {
            return;
        };
        for f in fields {
            counters.bump(f);
        }
    }

    /// Bump one counter by `n` for events that are not one-per-call.
    pub fn record_event_count(&self, field: &str, n: u64) {
        if let Some(counters) = &self.counters {
            counters.bump_count(field, n);
        }
    }

    /// Record how long an integrity check took.
    pub fn record_verify_duration(&self, ms: u128) {
        if let Some(h) = &self.verify_duration {
            h.record(ms as f64, &[]);
        }
    }

    /// Record how long ago the last integrity check finished, so a dashboard
    /// can alert when checks stop happening.
    pub fn record_verify_age(&self, last_verified: Option<&str>) {
        let Some(meter) = &self.meter else {
            return;
        };
        let Some(secs) = last_verified.and_then(verify_age_secs) else {
            return;
        };
        meter
            .f64_gauge("reposilo.verify.age_seconds")
            .with_description("Seconds since the last completed integrity check")
            .build()
            .record(secs, &[]);
    }

    /// A host was paused by a rate-limit signal. Labelled by host so a
    /// dashboard can show which forge is throttling.
    pub fn remote_rate_limited(&self, host: &str) {
        if let Some(counters) = &self.counters {
            counters.remote_rate_limited.add(1, &[KeyValue::new("host", host.to_string())]);
        }
    }

    /// A request was skipped because its host was rate-limited past the wait
    /// limit (it will be retried on the next scheduler pass).
    pub fn remote_requests_skipped(&self, host: &str) {
        if let Some(counters) = &self.counters {
            counters
                .remote_requests_skipped
                .add(1, &[KeyValue::new("host", host.to_string())]);
        }
    }

    /// Record the current index-derived gauges. Called on a timer by the
    /// server; the meter provider's periodic reader does the actual export.
    pub fn record_snapshot(&self, totals: &Totals) {
        let Some(meter) = &self.meter else {
            return;
        };
        let gauge = |name: &'static str, desc: &'static str, v: u64| {
            meter
                .u64_gauge(name)
                .with_description(desc)
                .build()
                .record(v, &[]);
        };
        gauge("reposilo.repos", "Archived repositories", totals.repos);
        gauge("reposilo.snapshots", "Archived branch snapshots", totals.snapshots);
        gauge("reposilo.releases", "Archived releases", totals.releases);
        gauge("reposilo.repos.dead", "Repositories with a dead remote", totals.dead);
        gauge("reposilo.repos.unavailable", "Repositories temporarily unreachable", totals.unavailable);
        gauge("reposilo.repos.untagged", "Repositories with no tags", totals.untagged);
        gauge(
            "reposilo.http.cache_hits",
            "Forge API responses revalidated from the conditional cache",
            totals.cache_hits,
        );
        gauge(
            "reposilo.http.cache_misses",
            "Forge API responses fetched fresh",
            totals.cache_misses,
        );
        gauge("reposilo.storage.bytes", "Total stored archive bytes", totals.archive_bytes);
        gauge("reposilo.storage.snapshot_bytes", "Bytes in branch snapshots", totals.snapshot_bytes);
        gauge("reposilo.storage.release_bytes", "Bytes in release archives", totals.release_bytes);
        gauge("reposilo.storage.asset_bytes", "Bytes in downloaded release assets", totals.asset_bytes);
    }
}

/// Seconds since an RFC3339 timestamp (None if it does not parse).
fn verify_age_secs(rfc3339: &str) -> Option<f64> {
    use time::format_description::well_known::Rfc3339;
    let t = time::OffsetDateTime::parse(rfc3339, &Rfc3339).ok()?;
    Some((time::OffsetDateTime::now_utc() - t).as_seconds_f64().max(0.0))
}

fn resource(cfg: &Config) -> Resource {
    Resource::builder()
        .with_service_name(cfg.otel.service_name.clone())
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Build providers from config. Never fails the caller: a broken exporter
/// endpoint logs and leaves telemetry disabled.
pub fn init(cfg: &Config) -> Telemetry {
    if !cfg.otel.enabled {
        return Telemetry::disabled();
    }
    match build(cfg) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("telemetry init failed: {e:#}");
            Telemetry::disabled()
        }
    }
}

fn build(cfg: &Config) -> Result<Telemetry> {
    let base = cfg.otel.endpoint.trim().trim_end_matches('/');
    if base.is_empty() {
        anyhow::bail!("otel endpoint is empty");
    }
    let resource = resource(cfg);

    let logger = if cfg.otel.logs {
        let exporter = LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{base}/v1/logs"))
            .build()
            .context("build OTLP log exporter")?;
        Some(
            SdkLoggerProvider::builder()
                .with_resource(resource.clone())
                .with_batch_exporter(exporter)
                .build(),
        )
    } else {
        None
    };

    let (meter_provider, meter, counters, verify_duration) = if cfg.otel.metrics {
        let exporter = MetricExporter::builder()
            .with_http()
            .with_endpoint(format!("{base}/v1/metrics"))
            .with_temporality(Temporality::Cumulative)
            .build()
            .context("build OTLP metric exporter")?;
        let reader = PeriodicReader::builder(exporter)
            .with_interval(Duration::from_secs(cfg.otel.interval_secs.max(10)))
            .build();
        let provider = SdkMeterProvider::builder()
            .with_resource(resource)
            .with_reader(reader)
            .build();
        let meter = provider.meter("reposilo");
        let counters = Counters::new(&meter);
        let verify_duration = meter
            .f64_histogram("reposilo.verify.duration_ms")
            .with_description("Wall time of an integrity check")
            .build();
        (Some(provider), Some(meter), Some(counters), Some(verify_duration))
    } else {
        (None, None, None, None)
    };

    Ok(Telemetry { logger, meter_provider, meter, counters, verify_duration })
}

/// Install the global tracing subscriber, bridging records into the logger
/// provider when OTel logs are enabled.
pub fn install_subscriber(telemetry: &Telemetry) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    let fmt_filter = || {
        EnvFilter::from_default_env().add_directive("info".parse().expect("valid filter directive"))
    };

    match telemetry.logger() {
        Some(provider) => {
            // Prevent a telemetry-induced-telemetry loop: logs from the HTTP
            // stack the exporter itself uses would otherwise feed back in.
            let otel_filter = EnvFilter::new("info")
                .add_directive("hyper=off".parse().expect("valid"))
                .add_directive("tonic=off".parse().expect("valid"))
                .add_directive("h2=off".parse().expect("valid"))
                .add_directive("reqwest=off".parse().expect("valid"));
            let otel_layer = OpenTelemetryTracingBridge::new(provider).with_filter(otel_filter);
            let fmt_layer = tracing_subscriber::fmt::layer().with_filter(fmt_filter());
            tracing_subscriber::registry().with(otel_layer).with(fmt_layer).init();
        }
        None => {
            let fmt_layer = tracing_subscriber::fmt::layer().with_filter(fmt_filter());
            tracing_subscriber::registry().with(fmt_layer).init();
        }
    }
}
