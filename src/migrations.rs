//! On-disk repo manifest migrations.
//!
//! The archive tree is the source of truth, so migrations are forward-only and
//! additive: each step fills in a field newer code expects and never removes
//! anything. A manifest records the schema it was last written with, so an old
//! archive can be brought up to [`SCHEMA_VERSION`] on demand from the settings
//! page, the CLI, or a refresh.
//!
//! A manifest from a newer schema than this build is left untouched. Running an
//! old binary against a newer archive must not downgrade it.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::archiver::Archiver;
use crate::types::RepoManifest;

/// Current on-disk repo manifest schema. Bump this whenever a new derived
/// field is added that existing archives should be backfilled with, and add the
/// matching step to [`migrate_manifest`].
pub const SCHEMA_VERSION: u32 = 1;

/// Outcome of a migration run.
#[derive(Debug, Default, Serialize)]
pub struct MigrationReport {
    pub scanned: usize,
    pub migrated: usize,
    pub errors: Vec<String>,
}

/// Bring one repo manifest up to the current schema. Returns true when the
/// manifest changed and should be written back.
///
/// With `dry_run`, nothing is fetched or written; the result only reports
/// whether the manifest is behind.
pub async fn migrate_manifest(
    archiver: &Archiver,
    manifest: &mut RepoManifest,
    repo_dir: &Path,
    dry_run: bool,
) -> bool {
    if manifest.schema > SCHEMA_VERSION {
        tracing::warn!(
            schema = manifest.schema,
            current = SCHEMA_VERSION,
            dir = %repo_dir.display(),
            "manifest is newer than this build; leaving it untouched"
        );
        return false;
    }
    if dry_run {
        return manifest.schema < SCHEMA_VERSION;
    }

    let before = manifest.schema;

    // 0 -> 1: card colors. Fetch a missing owner avatar when icons are enabled,
    // then derive the tint from whatever icon is now on disk.
    if manifest.schema < 1 {
        match manifest.origin.clone() {
            Some(origin) => archiver.backfill_card_color(manifest, repo_dir, &origin).await,
            None => archiver.derive_auto_color(manifest, repo_dir).await,
        }
        manifest.schema = 1;
    }

    manifest.schema != before
}

/// Migrate every repo in `repos` (rel, dir). With `dry_run`, nothing is fetched
/// or written; the report just counts what would change.
pub async fn migrate_all(
    archiver: &Archiver,
    repos: &[(String, PathBuf)],
    dry_run: bool,
) -> MigrationReport {
    let mut report = MigrationReport::default();
    for (rel, dir) in repos {
        report.scanned += 1;
        let manifest_path = dir.join("repo.json");
        let mut manifest: RepoManifest = match crate::types::read_json(&manifest_path) {
            Ok(m) => m,
            Err(e) => {
                report.errors.push(format!("{rel}: {e:#}"));
                continue;
            }
        };
        if migrate_manifest(archiver, &mut manifest, dir, dry_run).await {
            report.migrated += 1;
            if !dry_run {
                if let Err(e) = crate::types::write_json(&manifest_path, &manifest) {
                    report.errors.push(format!("{rel}: write failed: {e:#}"));
                }
            }
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn manifest_at(schema: u32, origin: Option<&str>) -> RepoManifest {
        let mut v = serde_json::json!({
            "forge": "github",
            "name": "demo",
            "added": "2025-01-01T00:00:00Z",
            "default_branch": "main",
            "schema": schema,
        });
        if let Some(o) = origin {
            v["origin"] = serde_json::Value::String(o.to_string());
        }
        serde_json::from_value(v).unwrap()
    }

    fn write_png(path: &Path, rgb: [u8; 3]) {
        let img = image::RgbaImage::from_pixel(16, 16, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(path, buf.into_inner()).unwrap();
    }

    #[tokio::test]
    async fn backfills_auto_color_from_an_existing_icon() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("owner-demo");
        std::fs::create_dir_all(&dir).unwrap();
        write_png(&dir.join("icon"), [220, 40, 30]);
        let mut m = manifest_at(0, Some("https://github.com/owner/demo"));
        let archiver = Archiver::new(Config::default());

        let changed = migrate_manifest(&archiver, &mut m, &dir, false).await;

        assert!(changed, "a schema 0 manifest should be migrated");
        assert_eq!(m.schema, SCHEMA_VERSION);
        let hex = m.auto_color.expect("icon present, so a tint should be derived");
        assert!(hex.starts_with('#') && hex.len() == 7, "got {hex}");
    }

    #[tokio::test]
    async fn a_newer_manifest_is_never_downgraded() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("owner-demo");
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = manifest_at(SCHEMA_VERSION + 1, None);
        let archiver = Archiver::new(Config::default());

        let changed = migrate_manifest(&archiver, &mut m, &dir, false).await;

        assert!(!changed);
        assert_eq!(m.schema, SCHEMA_VERSION + 1);
    }

    #[tokio::test]
    async fn dry_run_reports_without_mutating() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("owner-demo");
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = manifest_at(0, None);
        let archiver = Archiver::new(Config::default());

        let changed = migrate_manifest(&archiver, &mut m, &dir, true).await;

        assert!(changed, "dry run should still report the pending migration");
        assert_eq!(m.schema, 0, "dry run must not mutate the manifest");
        assert!(m.auto_color.is_none());
    }
}
