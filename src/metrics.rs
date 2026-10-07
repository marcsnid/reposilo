//! Lightweight, always-on stats plus an optional OTLP/HTTP exporter.
//!
//! Stats are per-day counters persisted to `<archive>/metrics.json`, capped to
//! a rolling window so the file can't grow forever. The built-in stats page
//! reads them directly; if `[otel] enabled = true` a background task also ships
//! them to an OpenTelemetry collector over OTLP/HTTP (JSON).

use std::collections::BTreeMap;
use std::path::Path;

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
    /// Integrity checks that completed (clean or with problems).
    pub verify_runs: u64,
    /// Individual integrity problems found across runs.
    pub verify_problems: u64,
    /// Integrity checks that could not complete.
    pub verify_fail: u64,
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
        self.verify_runs += other.verify_runs;
        self.verify_problems += other.verify_problems;
        self.verify_fail += other.verify_fail;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub started_at: String,
    /// When the last full integrity check finished (RFC3339), if ever.
    pub last_verified_at: Option<String>,
    /// "YYYY-MM-DD" -> counters (BTreeMap keeps them date-ordered).
    pub days: BTreeMap<String, DayStats>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self { started_at: now_rfc3339(), last_verified_at: None, days: BTreeMap::new() }
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
        self.bump_by(field, 1);
    }

    /// Bump one named counter by `n` (matches the `DayStats` fields).
    pub fn bump_by(&mut self, field: &str, n: u64) {
        let d = self.today();
        match field {
            "refresh_ok" => d.refresh_ok += n,
            "refresh_fail" => d.refresh_fail += n,
            "add_ok" => d.add_ok += n,
            "add_fail" => d.add_fail += n,
            "new_releases" => d.new_releases += n,
            "new_snapshots" => d.new_snapshots += n,
            "remote_gone" => d.remote_gone += n,
            "verify_runs" => d.verify_runs += n,
            "verify_problems" => d.verify_problems += n,
            "verify_fail" => d.verify_fail += n,
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
        m.bump_by("verify_problems", 3);
        m.bump("verify_runs");
        m.bump("verify_fail");
        let s = m.sums();
        assert_eq!(s.refresh_ok, 2);
        assert_eq!(s.refresh_fail, 1);
        assert_eq!(s.new_releases, 1);
        assert_eq!(s.verify_runs, 1);
        assert_eq!(s.verify_problems, 3);
        assert_eq!(s.verify_fail, 1);

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

}