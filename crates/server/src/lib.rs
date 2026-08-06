//! Go-Notes — a self-hosted, Obsidian-like notes server.
//!
//! The architectural rule that shapes everything here: **the markdown files on
//! disk are the source of truth**. Postgres holds a derived index — the link
//! graph, tags, full-text search, folder collapse state — and every table in
//! `migrations/0002_vault_index.sql` can be truncated and rebuilt by rescanning
//! the filesystem. Only `users` and `sessions` are authoritative in the database.
//!
//! That rule is why the filesystem watcher exists (a note edited over SSH is
//! just as valid as one edited in the browser), and why every write path does
//! the filesystem operation first and the database update second.

pub mod auth;
pub mod chunk;
pub mod config;
pub mod db;
pub mod embed;
pub mod error;
pub mod markdown;
pub mod routes;
pub mod state;
pub mod vault;
pub mod web;
