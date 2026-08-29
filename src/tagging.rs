//! Shared auto-tagging logic (CLI + web UI button both use this).

use crate::config::Config;
use crate::index::RepoEntry;
use crate::llm::{LlmTagger, TagTarget};
use crate::types::write_json;

pub struct AutotagEntryResult {
    pub repo: String,
    pub new_tags: Vec<String>,
}

pub struct AutotagReport {
    pub results: Vec<AutotagEntryResult>,
    pub changed: usize,
    pub failed: usize,
}

/// Run batch auto-tagging over repos. Writes manifests unless dry_run.
pub async fn run_autotag(cfg: &Config, repos: &[RepoEntry], dry_run: bool) -> AutotagReport {
    let tagger = LlmTagger::new(&cfg.llm);
    let mut report = AutotagReport { results: Vec::new(), changed: 0, failed: 0 };
    for batch in repos.chunks(tagger.batch_size()) {
        let targets: Vec<TagTarget> = batch
            .iter()
            .map(|r| TagTarget {
                name: r.manifest.name.clone(),
                description: r.manifest.description.clone(),
                notes: r.manifest.notes.clone(),
                readme_excerpt: readme_excerpt(r),
                existing_tags: r.manifest.tags.clone(),
            })
            .collect();
        let results = match tagger.tag_batch(&targets).await {
            Ok(r) => r,
            Err(_) => {
                report.failed += 1;
                continue;
            }
        };
        for res in results {
            let Some(repo) = batch.iter().find(|r| r.manifest.name == res.name || r.rel == res.name) else {
                continue;
            };
            let new: Vec<String> = res
                .tags
                .iter()
                .filter(|t| !repo.manifest.tags.iter().any(|e| e.eq_ignore_ascii_case(t)))
                .take(cfg.llm.max_tags)
                .cloned()
                .collect();
            if new.is_empty() {
                continue;
            }
            if !dry_run {
                let mut m = repo.manifest.clone();
                m.tags.extend(new.iter().cloned());
                m.tags.sort();
                m.tags.dedup();
                let _ = write_json(&repo.dir.join("repo.json"), &m);
                report.changed += 1;
            }
            report.results.push(AutotagEntryResult { repo: res.name, new_tags: new });
        }
    }
    report
}

fn readme_excerpt(r: &RepoEntry) -> Option<String> {
    std::fs::read_to_string(r.dir.join("README.md"))
        .ok()
        .map(|t| t.chars().take(2000).collect::<String>())
        .or_else(|| {
            r.branch_snapshots
                .first()
                .or_else(|| r.releases.first())
                .and_then(|e| crate::files::readme_from_archive(&e.dir.join(&e.sidecar.zip.file)))
                .map(|(_, text, _)| text.chars().take(2000).collect::<String>())
        })
}
