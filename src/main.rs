//! reposilo CLI surface: init, add, list, refresh, reindex, autotag.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

use reposilo::archiver::Archiver;
use reposilo::config::{default_config_path, Config};
use reposilo::index::{Index, RepoEntry};



#[derive(Parser)]
#[command(name = "reposilo", version, about = "Archive and index git repositories")]
struct Cli {
    /// Path to config file (default: ~/.config/reposilo/config.toml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum UserCmd {
    Add {
        name: String,
        /// Password (avoid shell history; consider env: REPOSILO_PASSWORD)
        #[arg(long)]
        password: String,
    },
    Remove { name: String },
    List,
}

#[derive(Subcommand)]
enum Command {
    /// Write a default config file
    Init {
        /// Archive tree root
        #[arg(long)]
        root: PathBuf,
    },
    /// Add and archive a repository
    Add {
        /// Repository URL (https, ssh, file://, or local path)
        url: String,
        /// Tag to apply (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
        /// Free-form note stored in repo.json
        #[arg(long)]
        note: Option<String>,
    },
    /// Refresh a repo (or all) from its remote
    Refresh {
        /// Repo path relative to archive root, or "all"
        #[arg(default_value = "all")]
        repo: String,
    },
    /// List archived repositories
    List {
        /// Tag filter (repeatable, intersection)
        #[arg(long = "tag")]
        tags: Vec<String>,
        /// Substring query
        #[arg(long)]
        query: Option<String>,
    },
    /// Rebuild the index from disk
    Reindex {
        /// Also write catalog.json (read-only human-friendly view)
        #[arg(long)]
        catalog: bool,
    },
    /// Import a directory of zipped repositories into the archive
    Import {
        /// Directory containing zip files to import
        dir: PathBuf,
        /// Show what would be imported without moving anything
        #[arg(long)]
        dry_run: bool,
        /// Copy zips instead of moving them
        #[arg(long)]
        copy: bool,
        /// Do not import zips with no detectable origin (skip them)
        #[arg(long)]
        no_unknown: bool,
        /// Tags to apply to imported repos (repeatable)
        #[arg(long = "tag", value_name = "TAG")]
        tags: Vec<String>,
    },
    /// Rename owner/repo folder pairs to flat owner-repo folders (fork-safe)
    MigrateFolders {
        /// Show what would be renamed without renaming
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete leftover shallow.git stores from archives made before zip-only
    /// storage (reclaims disk; zips and metadata are untouched)
    PruneShallow {
        /// Show what would be deleted without deleting
        #[arg(long)]
        dry_run: bool,
    },
    /// Verify archived zips against their sidecars (sha256 + size)
    Verify {
        /// Repo path relative to archive root, or "all"
        #[arg(default_value = "all")]
        repo: String,
    },
    /// Manage user accounts for the web UI
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Start the HTTP API server (+ background scheduler)
    Serve {
        /// Disable the background scheduler
        #[arg(long)]
        no_scheduler: bool,
        /// Listen address, e.g. 0.0.0.0:8765 (overrides config and REPOSILO_BIND)
        #[arg(long)]
        bind: Option<String>,
    },
    /// Tag repos with a local llama.cpp server
    Autotag {
        /// Re-tag repos that already have tags too
        #[arg(long)]
        all: bool,
        /// Only print suggestions, do not write
        #[arg(long)]
        dry_run: bool,
        /// Max number of repos to process
        #[arg(long)]
        limit: Option<usize>,
    },
}

fn load_config(path: Option<&PathBuf>) -> Result<(Config, PathBuf)> {
    let p = path.cloned().unwrap_or_else(default_config_path);
    let mut cfg = Config::load(&p)?;
    cfg.with_absolute_root();
    Ok((cfg, p))
}

fn archive_root(cfg: &Config) -> PathBuf {
    PathBuf::from(&cfg.archive.root)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Init { root } => {
            let path = cli.config.clone().unwrap_or_else(default_config_path);
            if path.exists() {
                bail!("config already exists: {} (delete it or pass --config)", path.display());
            }
            let mut cfg = Config::default();
            cfg.archive.root = root.to_string_lossy().into_owned();
            cfg.save(&path)?;
            fs::create_dir_all(&cfg.archive.root)
                .with_context(|| format!("cannot create archive root {}", cfg.archive.root))?;
            println!("wrote {} (archive root: {})", path.display(), cfg.archive.root);
            Ok(())
        }

        Command::Add { url, tags, note } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let archiver = Archiver::new(cfg.clone());
            let dir = archiver.add_repo(&url, &tags, note).await?;
            println!("archived {} → {}", url, dir.display());
            let index = Index::load(&archive_root(&cfg))?;
            if let Some(r) = index.repos.iter().find(|r| r.dir == dir) {
                print_repo(r);
            }
            Ok(())
        }

        Command::Refresh { repo } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let index = Index::load(&archive_root(&cfg))?;
            let archiver = Archiver::new(cfg);
            let dirs: Vec<(String, PathBuf)> = if repo == "all" {
                index.repos.iter().map(|r| (r.rel.clone(), r.dir.clone())).collect()
            } else {
                let r = index.find(&repo).with_context(|| format!("no such repo: {repo}"))?;
                vec![(r.rel.clone(), r.dir.clone())]
            };
            for (rel, dir) in dirs {
                match archiver.refresh_repo(&dir).await {
                    Ok(s) => {
                        let mut parts = vec![];
                        if s.new_branch_snapshot {
                            parts.push("new branch snapshot".to_string());
                        }
                        if let Some(v) = &s.new_release {
                            parts.push(format!("new release {v}"));
                        }
                        for p in &s.pruned {
                            parts.push(format!("pruned {p}"));
                        }
                        if parts.is_empty() {
                            println!("{rel}: up to date");
                        } else {
                            println!("{rel}: {}", parts.join(", "));
                        }
                    }
                    Err(e) => eprintln!("{rel}: refresh failed: {e:#}"),
                }
            }
            Ok(())
        }

        Command::List { tags, query } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let index = Index::load(&archive_root(&cfg))?;
            let repos = index.filter(&tags, query.as_deref());
            if repos.is_empty() {
                println!("no repos match");
                return Ok(());
            }
            for r in repos {
                print_repo(r);
            }
            Ok(())
        }

        Command::Reindex { catalog } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let root = archive_root(&cfg);
            let index = Index::load(&root)?;
            let tags = index.all_tags();
            println!(
                "{} repos, {} snapshots, {} distinct tags",
                index.repos.len(),
                index.snapshot_count(),
                tags.len()
            );
            for (t, n) in tags {
                println!("  #{t} ({n})");
            }
            if catalog {
                let repos: Vec<_> = index
                    .repos
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "path": r.rel,
                            "origin": r.manifest.origin,
                            "forge": r.manifest.forge,
                            "tags": r.manifest.tags,
                            "branch_snapshots": r.branch_snapshots.len(),
                            "releases": r.releases.len(),
                        })
                    })
                    .collect();
                let catalog_json = serde_json::json!({
                    "generated_at": reposilo::archiver::now_rfc3339(),
                    "repos": repos,
                });
                let path = root.join("catalog.json");
                reposilo::types::write_json(&path, &catalog_json)?;
                println!("wrote {}", path.display());
            }
            Ok(())
        }

        Command::Import { dir, dry_run, copy, no_unknown, tags } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let scan = reposilo::importer::scan_dir(&dir)?;
            if scan.rows.is_empty() {
                println!("no zips found in {}", dir.display());
                return Ok(());
            }
            println!("{} zip(s) found in {}:", scan.rows.len(), dir.display());
            for r in &scan.rows {
                let origin = r.detected_origin.as_deref().unwrap_or("-");
                let name = r.detected_name.as_deref().unwrap_or("-");
                println!("  {:40} → {:<40} [{}] {}", r.file_name, origin, name, r.evidence.join("/"));
            }
            if dry_run {
                println!("dry run: nothing changed");
                return Ok(());
            }
            let archiver = Archiver::new(cfg.clone());
            let mut imported = 0usize;
            let mut parked = 0usize;
            let mut failed = 0usize;
            for r in &scan.rows {
                let unknown = r.detected_origin.is_none();
                if unknown && no_unknown {
                    println!("  SKIP {} (unidentified)", r.file_name);
                    continue;
                }
                let o = reposilo::importer::import_one(
                    &cfg,
                    r,
                    r.detected_origin.as_deref(),
                    &tags,
                    unknown,
                    copy,
                )
                .await;
                match o.action.as_str() {
                    "failed" => { failed += 1; println!("  FAIL  {}: {}", o.file, o.detail); }
                    "unknown-parked" => { parked += 1; println!("  PARK  {} → {}", o.file, o.repo); }
                    _ => { imported += 1; println!("  OK    {} → {}", o.file, o.repo); }
                }
            }
            println!("imported {imported}, parked {parked} unknown, {failed} failed");
            let _ = archiver; // Archiver import kept for symmetry; unused
            let index = Index::load(&archive_root(&cfg))?;
            println!("index now holds {} repos", index.repos.len());
            Ok(())
        }

        Command::MigrateFolders { dry_run } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let index = Index::load(&archive_root(&cfg))?;
            let mut count = 0usize;
            for r in &index.repos {
                // only the manifest knows if this is an owner/repo pair
                let Some(origin) = &r.manifest.origin else { continue };
                let Ok(info) = reposilo::forge::detect(origin) else { continue };
                let owner = reposilo::archiver::sanitize(&info.owner);
                let name = reposilo::archiver::sanitize(&info.name);
                let dir_name = r.dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                let parent_name = r.dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or_default();
                if dir_name != name || parent_name != owner {
                    continue; // already flat, a category folder, or an import
                }
                let grandparent = r.dir.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf());
                let Some(dest) = grandparent else { continue };
                let new_dir = dest.join(format!("{owner}-{name}"));
                if new_dir.exists() {
                    println!("SKIP {} (target exists)", r.rel);
                    continue;
                }
                println!("{} {} -> {}", if dry_run { "would rename" } else { "rename" }, r.rel, new_dir.display());
                let old_parent = r.dir.parent().map(|p| p.to_path_buf());
                if !dry_run {
                    std::fs::rename(&r.dir, &new_dir)
                        .with_context(|| format!("cannot rename {}", r.dir.display()))?;
                    // sweep the now-empty owner folder
                    if let Some(p) = &old_parent {
                        if std::fs::read_dir(p).map(|mut it| it.next().is_none()).unwrap_or(false) {
                            let _ = std::fs::remove_dir(p);
                        }
                    }
                }
                count += 1;
            }
            if count == 0 {
                println!("no owner/repo pairs found: archive already uses flat folders");
            } else {
                println!("{} {} repo folder(s)", if dry_run { "would rename" } else { "renamed" }, count);
            }
            Ok(())
        }

        Command::PruneShallow { dry_run } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            if cfg.git.depth == 0 {
                bail!("git.depth = 0 (full mirror mode) keeps its git stores by design; nothing to prune");
            }
            let index = Index::load(&archive_root(&cfg))?;
            let mut total: u64 = 0;
            let mut count = 0usize;
            for r in &index.repos {
                let d = r.dir.join("shallow.git");
                if !d.is_dir() {
                    continue;
                }
                let size = dir_size(&d);
                println!("{:<40} {:>10}", r.rel, human_size(size));
                total += size;
                count += 1;
                if !dry_run {
                    std::fs::remove_dir_all(&d)
                        .with_context(|| format!("cannot remove {}", d.display()))?;
                }
            }
            if count == 0 {
                println!("no shallow.git stores found: archive is already zip-only");
            } else {
                println!(
                    "{} {} shallow.git store(s) ({})",
                    if dry_run { "would remove" } else { "removed" },
                    count,
                    human_size(total)
                );
            }
            Ok(())
        }

        Command::Verify { repo } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let root = archive_root(&cfg);
            let only = if repo == "all" { None } else { Some(repo.as_str()) };
            let mut last = 0usize;
            let report = reposilo::verify::verify_archive(&root, only, |p| {
                if p.total > 0 && p.done != last && (p.done == p.total || p.done % 25 == 0) {
                    eprint!("\rverifying {}/{}", p.done, p.total);
                    last = p.done;
                }
            })?;
            if last > 0 {
                eprintln!("\r{:40}", "");
            }
            println!("{}", report.summary());
            for p in &report.problems {
                println!("  {} {} [{}] {}", p.repo, p.file, p.kind, p.detail);
            }
            if !report.is_clean() {
                std::process::exit(1);
            }
            Ok(())
        }

        Command::User { cmd } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            let root = archive_root(&cfg);
            match cmd {
                UserCmd::Add { name, password } => {
                    let pw = std::env::var("REPOSILO_PASSWORD").unwrap_or(password);
                    reposilo::auth::add_user(&root, &name, &pw)?;
                    println!("user '{name}' added: login at the web UI now works");
                }
                UserCmd::Remove { name } => {
                    reposilo::auth::remove_user(&root, &name)?;
                    println!("user '{name}' removed");
                }
                UserCmd::List => {
                    let users = reposilo::auth::list_users(&root).unwrap_or_default();
                    if users.is_empty() {
                        println!("no users yet; the web UI is open (add one with: reposilo user add <name> --password <pw>)");
                    } else {
                        for u in users {
                            println!("  {u}");
                        }
                    }
                }
            }
            Ok(())
        }

        Command::Serve { no_scheduler, bind } => {
            let (mut cfg, config_path) = load_config(cli.config.as_ref())?;
            // precedence: --bind > REPOSILO_BIND > config
            let bind = bind
                .or_else(|| std::env::var("REPOSILO_BIND").ok())
                .map(|b| b.trim().to_string())
                .filter(|b| !b.is_empty());
            if let Some(b) = bind {
                cfg.server.bind = b;
            }
            reposilo::server::serve(cfg, Some(config_path), no_scheduler).await
        }

        Command::Autotag { all, dry_run, limit } => {
            let (cfg, _) = load_config(cli.config.as_ref())?;
            if !cfg.llm.enabled {
                bail!("llm auto-tagging is disabled in config ([llm] enabled = false)");
            }
            let index = Index::load(&archive_root(&cfg))?;
            let targets: Vec<RepoEntry> = index
                .repos
                .into_iter()
                .filter(|r| all || r.manifest.tags.is_empty())
                .take(limit.unwrap_or(usize::MAX))
                .collect();
            if targets.is_empty() {
                println!("no untagged repos");
                return Ok(());
            }
            let report = reposilo::tagging::run_autotag(&cfg, &targets, dry_run).await;
            for r in &report.results {
                if !r.new_tags.is_empty() {
                    println!("  {}: {}", r.repo, r.new_tags.join(", "));
                }
            }
            if report.failed > 0 {
                eprintln!("{} batch(es) failed", report.failed);
            }
            if dry_run {
                println!("dry run: nothing written");
            } else {
                println!("updated tags on {} repo(s)", report.changed);
            }
            Ok(())
        }
    }
}

fn dir_size(dir: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                total += dir_size(&p);
            } else if let Ok(md) = e.metadata() {
                total += md.len();
            }
        }
    }
    total
}

fn human_size(b: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

fn print_repo(r: &RepoEntry) {
    let tags = if r.manifest.tags.is_empty() {
        String::new()
    } else {
        format!(" [{}]", r.manifest.tags.join(", "))
    };
    println!("{} ({}){}", r.rel, r.manifest.forge, tags);
    if let Some(b) = r.branch_snapshots.first() {
        let sha = &b.sidecar.commit[..b.sidecar.commit.len().min(7)];
        println!(
            "    {}: {} snapshot(s), latest {} ({})",
            r.manifest.default_branch,
            r.branch_snapshots.len(),
            sha,
            &b.sidecar.archived_at[..b.sidecar.archived_at.len().min(10)]
        );
    }
    if let Some(rel) = r.releases.first() {
        println!("    release: {}", rel.sidecar.version.as_deref().unwrap_or(&rel.sidecar.r#ref));
    }
}