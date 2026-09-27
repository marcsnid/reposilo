//! Optional OpenTelemetry export: logs bridged from `tracing`, and metrics.
//!
//! Providers are built once after config load, the logging bridge is installed
//! into the global tracing subscriber, and the handles are handed to the rest
//! of the program (`AppState`) so they stay alive and can be flushed on exit.

use std::time::Duration;

use anyhow::{Context, Result};
use opentelemetry::metrics::{Meter, MeterProvider};
use opentelemetry::KeyValue;
use opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge;
use opentelemetry_otlp::{LogExporter, MetricExporter, WithExportConfig};
use opentelemetry_sdk::logs::SdkLoggerProvider;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider, Temporality};
use opentelemetry_sdk::Resource;

use crate::config::Config;
use crate::metrics::{Metrics, Totals};

/// Live OpenTelemetry providers (or nothing when `[otel] enabled = false`).
#[derive(Clone)]
pub struct Telemetry {
    logger: Option<SdkLoggerProvider>,
    meter_provider: Option<SdkMeterProvider>,
    meter: Option<Meter>,
}

impl Telemetry {
    pub fn disabled() -> Self {
        Self { logger: None, meter_provider: None, meter: None }
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

    /// Record the current archive values. Called on a timer by the server;
    /// the meter provider's periodic reader does the actual export.
    pub fn record_snapshot(&self, metrics: &Metrics, totals: &Totals) {
        let Some(meter) = &self.meter else {
            return;
        };
        let sums = metrics.sums();
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
        gauge("reposilo.refresh.success", "Successful refreshes", sums.refresh_ok);
        gauge("reposilo.refresh.failure", "Failed refreshes", sums.refresh_fail);
        gauge("reposilo.add.success", "Successful adds", sums.add_ok);
        gauge("reposilo.add.failure", "Failed adds", sums.add_fail);
        gauge("reposilo.releases.new", "New releases archived", sums.new_releases);
        gauge("reposilo.snapshots.new", "New branch snapshots", sums.new_snapshots);
        gauge("reposilo.remote.gone", "Remotes observed unreachable", sums.remote_gone);
    }
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

    let (meter_provider, meter) = if cfg.otel.metrics {
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
        (Some(provider), Some(meter))
    } else {
        (None, None)
    };

    Ok(Telemetry { logger, meter_provider, meter })
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
