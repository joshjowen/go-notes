//! Keeping the index honest when files change behind the app's back.
//!
//! A vault is a directory of ordinary markdown files, so the app is not the only
//! thing that can write to it — `git pull`, an SSH session, a sync client and a
//! backup restore are all legitimate authors. This watcher is what makes those
//! edits show up in the browser without a restart.
//!
//! The event handling deliberately ignores *what kind* of change notify reports.
//! Interpreting create/modify/remove/rename-from/rename-to correctly across
//! platforms and editors is notoriously fiddly — an editor that saves by writing
//! a temp file and renaming produces a different event sequence from one that
//! truncates in place. Instead, every event for a markdown file collapses to one
//! question: *does that file exist right now?* If it does, reindex it; if it does
//! not, forget it. Both answers are idempotent, so a duplicated or misordered
//! event costs nothing.
//!
//! Anything that is not a markdown file — a new folder, a deleted directory, an
//! uploaded attachment — schedules a reconcile of that user's vault instead,
//! which is the same code path startup uses and is correct by construction.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use go_notes_shared::paths;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::{self, User};
use crate::vault::{index, Vault};

/// How long to wait for a burst of filesystem activity to settle.
///
/// Saving a note produces several events (create temp, write, rename); a
/// `git pull` produces hundreds. Half a second is long enough to collapse those
/// into one pass and short enough that an external edit appears to be live.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Holding this alive keeps the watch running; dropping it stops it.
pub struct VaultWatcher {
    _debouncer: Debouncer<notify::RecommendedWatcher, RecommendedCache>,
}

/// Starts watching `data_dir` and applying changes to the index.
pub fn spawn(pool: PgPool, data_dir: PathBuf) -> Result<VaultWatcher> {
    // One recursive watch over every vault, rather than one per user: users are
    // created at runtime, and re-registering watches as they appear would be a
    // second thing to keep in sync.
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating data directory {}", data_dir.display()))?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();

    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            Ok(events) => {
                let paths: Vec<PathBuf> = events
                    .into_iter()
                    .flat_map(|event| event.event.paths.clone())
                    .collect();
                if !paths.is_empty() {
                    // A send failure means the consumer task is gone, i.e. the
                    // server is shutting down. Nothing to do about it here.
                    let _ = tx.send(paths);
                }
            }
            Err(errors) => {
                for error in errors {
                    tracing::warn!(error = %error, "filesystem watch error");
                }
            }
        }
    })
    .context("creating filesystem watcher")?;

    debouncer
        .watch(&data_dir, RecursiveMode::Recursive)
        .with_context(|| format!("watching {}", data_dir.display()))?;

    let watch_root = data_dir.clone();
    tokio::spawn(async move {
        while let Some(paths) = rx.recv().await {
            if let Err(err) = apply_batch(&pool, &watch_root, paths).await {
                // A failure here must never kill the watcher: the next batch,
                // or the next restart's reconcile, will catch up.
                tracing::error!(error = ?err, "failed to apply filesystem changes");
            }
        }
        tracing::info!("filesystem watcher stopped");
    });

    tracing::info!(path = %data_dir.display(), "watching for external note changes");
    Ok(VaultWatcher {
        _debouncer: debouncer,
    })
}

/// What a batch of paths asks us to do.
#[derive(Default)]
struct Batch {
    /// Markdown files to look at, keyed by user.
    touched: HashMap<Uuid, HashSet<String>>,
    /// Users whose whole vault needs rescanning.
    reconcile: HashSet<Uuid>,
}

async fn apply_batch(pool: &PgPool, data_dir: &Path, paths: Vec<PathBuf>) -> Result<()> {
    // Loading every user once per batch beats a lookup per path; a deployment
    // has tens of users, not thousands, and batches can carry hundreds of paths.
    let users = db::list_users(pool).await?;
    let by_vault: HashMap<&str, &User> = users
        .iter()
        .map(|user| (user.vault_dir.as_str(), user))
        .collect();

    let mut batch = Batch::default();

    for path in paths {
        let Some((vault_dir, rel)) = split_vault_path(data_dir, &path) else {
            continue;
        };
        let Some(user) = by_vault.get(vault_dir.as_str()) else {
            // A directory under the data root that belongs to no user. Could be
            // a vault whose user was deleted, or something a person put there.
            continue;
        };

        // `.trash`, `.git`, `.obsidian` and our own `.note.md.<hex>.tmp` files.
        // The scan skips them, so reacting to them would only cause churn.
        if rel.split('/').any(|part| part.starts_with('.')) {
            continue;
        }

        if rel.is_empty() || !paths::has_md_extension(&rel) {
            // A folder, an attachment, or the vault root itself.
            batch.reconcile.insert(user.id);
        } else if paths::validate_rel_path(&rel).is_ok() {
            batch.touched.entry(user.id).or_default().insert(rel);
        }
    }

    for user in &users {
        if batch.reconcile.contains(&user.id) {
            // A full reconcile subsumes any individual note changes for this
            // user, so there is no point doing both.
            let vault = Vault::open(data_dir, &user.vault_dir)?;
            index::reconcile_vault(pool, user, &vault).await?;
            continue;
        }

        let Some(touched) = batch.touched.get(&user.id) else {
            continue;
        };
        let vault = Vault::open(data_dir, &user.vault_dir)?;

        for rel in touched {
            if let Err(err) = apply_one(pool, user, &vault, rel).await {
                tracing::warn!(user = %user.username, path = %rel, error = ?err, "could not apply change");
            }
        }
    }

    Ok(())
}

/// Reindexes a note, or forgets it if the file is gone.
async fn apply_one(pool: &PgPool, user: &User, vault: &Vault, rel: &str) -> Result<()> {
    let path = vault.resolve_note(rel)?;

    if !crate::vault::store::exists(&path).await {
        index::remove_note(pool, user.id, rel).await?;
        tracing::debug!(user = %user.username, path = %rel, "note removed externally");
        return Ok(());
    }

    // The hash comparison inside `index_if_changed` is what makes the app's own
    // writes free: it wrote the file, the watcher noticed, and the content
    // already matches what is indexed, so this returns immediately.
    if index::index_if_changed(pool, user, vault, &path).await? {
        tracing::info!(user = %user.username, path = %rel, "reindexed after external edit");
    }
    Ok(())
}

/// Splits an absolute path under the data root into `(vault_dir, rel_path)`.
///
/// Returns `None` for anything not inside the data root. `rel_path` is empty
/// when the path *is* a vault directory.
fn split_vault_path(data_dir: &Path, path: &Path) -> Option<(String, String)> {
    let rel = path.strip_prefix(data_dir).ok()?;

    let mut components = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => components.push(part.to_str()?.to_string()),
            _ => return None,
        }
    }

    let (vault_dir, rest) = components.split_first()?;
    Some((vault_dir.clone(), rest.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_paths_under_the_data_root() {
        let root = Path::new("/data/notes");

        assert_eq!(
            split_vault_path(root, Path::new("/data/notes/alice/Projects/A.md")),
            Some(("alice".into(), "Projects/A.md".into()))
        );
        // A vault directory itself, which means "rescan this user".
        assert_eq!(
            split_vault_path(root, Path::new("/data/notes/alice")),
            Some(("alice".into(), String::new()))
        );
        // Outside the data root entirely.
        assert_eq!(split_vault_path(root, Path::new("/etc/passwd")), None);
        assert_eq!(split_vault_path(root, Path::new("/data/notes")), None);
    }

    /// The events the app's own atomic writes generate must not be mistaken for
    /// notes, or every save would index a temp file and then delete it again.
    #[test]
    fn temporary_and_hidden_paths_are_recognisable() {
        let root = Path::new("/data/notes");
        let hidden = [
            "/data/notes/alice/.Note.md.a1b2c3d4.tmp",
            "/data/notes/alice/.trash/2026-01-01T00-00-00/Note.md",
            "/data/notes/alice/.git/objects/ab/cdef",
            "/data/notes/alice/.obsidian/workspace.json",
        ];
        for path in hidden {
            let (_, rel) = split_vault_path(root, Path::new(path)).unwrap();
            assert!(
                rel.split('/').any(|part| part.starts_with('.')),
                "{path} should be filtered as hidden"
            );
        }

        let (_, rel) = split_vault_path(root, Path::new("/data/notes/alice/Real.md")).unwrap();
        assert!(!rel.split('/').any(|part| part.starts_with('.')));
    }
}
