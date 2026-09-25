//! reposilo: archive and index git repositories.
//!
//! The archive tree on disk is the source of truth; the in-memory index is
//! fully rebuildable from it.

pub mod archiver;
pub mod auth;
pub mod config;
pub mod files;
pub mod forge;
pub mod forgeapi;
pub mod importer;
pub mod index;
pub mod llm;
pub mod metrics;
pub mod platform;
pub mod releaseapi;
pub mod server;
pub mod tagging;
pub mod types;
pub mod web;