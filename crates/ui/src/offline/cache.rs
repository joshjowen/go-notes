//! The local vault: what this device holds when the server is not there.
//!
//! Three things live here — the notes that have been opened on this device, the
//! last file tree the server sent, and the outbox of changes waiting to be
//! replayed. Everything is best-effort: a failure to read or write the cache is
//! logged and degrades the feature, never breaks the app, because a browser can
//! refuse storage (private windows, a full disk, a strict privacy setting) and
//! the editor must still work when it does.
//!
//! The cache belongs to one signed-in user at a time. Signing in as somebody
//! else wipes it before anything is written, so a shared browser never shows one
//! person another's notes — and signing out wipes it too.

use go_notes_shared::{Me, TreeNode};
use serde::de::DeserializeOwned;
use serde::Serialize;
use wasm_bindgen::JsValue;

use super::idb::{Db, STORE_META, STORE_NOTES};
use super::queue::{coalesce, PendingOp, QueuedOp};
use super::CachedNote;

const KEY_IDENTITY: &str = "identity";
const KEY_TREE: &str = "tree";
const KEY_OUTBOX: &str = "outbox";

thread_local! {
    /// One handle for the page. Opening the database is asynchronous and not
    /// free, and every call here would otherwise pay for it.
    static HANDLE: std::cell::RefCell<Option<Db>> = const { std::cell::RefCell::new(None) };
}

/// Opens the database, or returns `None` if this browser will not give us one.
pub async fn db() -> Option<Db> {
    if let Some(existing) = HANDLE.with(|handle| handle.borrow().clone()) {
        return Some(existing);
    }
    match Db::open().await {
        Ok(db) => {
            HANDLE.with(|handle| *handle.borrow_mut() = Some(db.clone()));
            Some(db)
        }
        Err(err) => {
            warn("offline storage is unavailable", &err);
            None
        }
    }
}

/// Whether local storage is working, for the status popover.
pub async fn is_available() -> bool {
    db().await.is_some()
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

pub async fn note(path: &str) -> Option<CachedNote> {
    read(STORE_NOTES, path).await
}

pub async fn put_note(note: &CachedNote) {
    write(STORE_NOTES, &note.path, note).await;
}

pub async fn remove_note(path: &str) {
    let Some(db) = db().await else { return };
    if let Err(err) = db.delete(STORE_NOTES, path).await {
        warn("could not remove a cached note", &err);
    }
}

pub async fn all_notes() -> Vec<CachedNote> {
    let Some(db) = db().await else {
        return Vec::new();
    };
    match db.all(STORE_NOTES).await {
        Ok(values) => values.iter().filter_map(decode).collect(),
        Err(err) => {
            warn("could not read the cached notes", &err);
            Vec::new()
        }
    }
}

/// Moves cached notes to follow a rename, including everything inside a folder.
pub async fn rename_notes(from: &str, to: &str) {
    for mut cached in all_notes().await {
        let moved = if cached.path == from {
            Some(to.to_string())
        } else {
            go_notes_shared::paths::rebase(&cached.path, from, to)
        };
        let Some(moved) = moved else { continue };

        remove_note(&cached.path).await;
        cached.title = go_notes_shared::paths::stem(&moved).to_string();
        cached.path = moved;
        put_note(&cached).await;
    }
}

/// Removes a note and anything beneath it, for a deleted folder.
pub async fn remove_under(path: &str) {
    for cached in all_notes().await {
        if cached.path == path || go_notes_shared::paths::is_within(&cached.path, path) {
            remove_note(&cached.path).await;
        }
    }
}

/// Drops cached notes the server's tree no longer lists — the counterpart to
/// every `put_note`, which nothing was previously taking back out.
///
/// Without this, a note deleted on another device stays in this device's
/// IndexedDB forever and keeps turning up in offline search, quickswitch and
/// backlinks, which all read `all_notes()` directly.
pub async fn retain_notes(keep: &[String]) {
    let cached: Vec<String> = all_notes().await.into_iter().map(|note| note.path).collect();
    let queue = outbox().await;

    for path in prune_targets(&cached, keep, &queue) {
        remove_note(&path).await;
    }
}

/// Which cached paths are safe to drop: not in `keep`, and not something a
/// queued operation still needs to exist locally before it can replay.
///
/// The exception matters: a tree fetched while a note created (or moved)
/// offline is still queued would otherwise delete the very thing the queued
/// write is trying to send — the server's tree does not know about it either,
/// precisely because the write has not reached it yet.
fn prune_targets(cached: &[String], keep: &[String], queue: &[QueuedOp]) -> Vec<String> {
    cached
        .iter()
        .filter(|path| !keep.iter().any(|kept| kept == *path))
        .filter(|path| !is_protected(path, queue))
        .cloned()
        .collect()
}

fn is_protected(path: &str, queue: &[QueuedOp]) -> bool {
    queue.iter().any(|queued| match &queued.op {
        PendingOp::CreateNote { path: subject, .. } | PendingOp::SaveNote { path: subject, .. } => {
            subject == path
        }
        PendingOp::MoveNote { to, .. } => to == path,
        // A folder move rebases every cached note beneath it before the op is
        // even queued (`vault::move_folder`), so what needs protecting here is
        // where those notes now live — `to`, not `from`.
        PendingOp::MoveFolder { to, .. } => {
            path == to || go_notes_shared::paths::is_within(path, to)
        }
        PendingOp::DeleteNote { .. } | PendingOp::CreateFolder { .. } | PendingOp::DeleteFolder { .. } => {
            false
        }
    })
}

// ---------------------------------------------------------------------------
// Tree and identity
// ---------------------------------------------------------------------------

pub async fn tree() -> Option<TreeNode> {
    read(STORE_META, KEY_TREE).await
}

pub async fn put_tree(tree: &TreeNode) {
    write(STORE_META, KEY_TREE, tree).await;
}

pub async fn identity() -> Option<Me> {
    read(STORE_META, KEY_IDENTITY).await
}

/// Records who the cache belongs to, wiping it first if that has changed.
pub async fn remember_identity(me: &Me) {
    if let Some(previous) = identity().await {
        if previous.username != me.username {
            forget_everything().await;
        }
    }
    write(STORE_META, KEY_IDENTITY, me).await;
}

/// Drops everything held for offline use. Called on sign-out, on a user switch,
/// and from the "forget local copy" command.
pub async fn forget_everything() {
    let Some(db) = db().await else { return };
    for store in [STORE_NOTES, STORE_META] {
        if let Err(err) = db.clear(store).await {
            warn("could not clear the local copy", &err);
        }
    }
}

// ---------------------------------------------------------------------------
// The outbox
// ---------------------------------------------------------------------------

pub async fn outbox() -> Vec<QueuedOp> {
    read::<Vec<QueuedOp>>(STORE_META, KEY_OUTBOX)
        .await
        .unwrap_or_default()
}

pub async fn write_outbox(ops: &[QueuedOp]) {
    write(STORE_META, KEY_OUTBOX, &ops.to_vec()).await;
}

/// Records a change to replay later, compacting the queue as it goes.
///
/// Returns the queue as it now stands, so the caller can update the pending
/// count without reading it back.
pub async fn enqueue(op: PendingOp) -> Vec<QueuedOp> {
    let mut ops = outbox().await;
    let next = ops.iter().map(|queued| queued.id).max().unwrap_or(0) + 1;
    ops.push(QueuedOp::new(next, op));

    let ops = coalesce(ops);
    write_outbox(&ops).await;
    ops
}

/// Removes one operation — it succeeded, or the user discarded it.
pub async fn drop_op(id: u64) -> Vec<QueuedOp> {
    let mut ops = outbox().await;
    ops.retain(|queued| queued.id != id);
    write_outbox(&ops).await;
    ops
}

/// Records why an operation could not be replayed, so it can be shown rather
/// than retried forever in silence.
pub async fn mark_failed(id: u64, reason: &str) -> Vec<QueuedOp> {
    let mut ops = outbox().await;
    for queued in ops.iter_mut() {
        if queued.id == id {
            queued.last_error = Some(reason.to_string());
        }
    }
    write_outbox(&ops).await;
    ops
}

// ---------------------------------------------------------------------------
// Plumbing
// ---------------------------------------------------------------------------

async fn read<T: DeserializeOwned>(store: &str, key: &str) -> Option<T> {
    let db = db().await?;
    match db.get(store, key).await {
        Ok(Some(value)) => decode(&value),
        Ok(None) => None,
        Err(err) => {
            warn("could not read from offline storage", &err);
            None
        }
    }
}

async fn write<T: Serialize>(store: &str, key: &str, value: &T) {
    let Some(db) = db().await else { return };
    let Ok(json) = serde_json::to_string(value) else {
        return;
    };
    if let Err(err) = db.put(store, key, &JsValue::from_str(&json)).await {
        warn("could not write to offline storage", &err);
    }
}

/// Values are stored as JSON strings rather than structured clones: one
/// serialisation format, shared with the API, and no second set of type
/// mappings to keep in step.
fn decode<T: DeserializeOwned>(value: &JsValue) -> Option<T> {
    let json = value.as_string()?;
    match serde_json::from_str(&json) {
        Ok(decoded) => Some(decoded),
        Err(err) => {
            // A record written by an older build that no longer parses is not
            // worth failing over; it will be replaced on the next write.
            web_sys::console::warn_1(&JsValue::from_str(&format!(
                "go-notes: discarding an unreadable cached record ({err})"
            )));
            None
        }
    }
}

fn warn(message: &str, err: &JsValue) {
    web_sys::console::warn_2(&JsValue::from_str(&format!("go-notes: {message}")), err);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(id: u64, op: PendingOp) -> QueuedOp {
        QueuedOp::new(id, op)
    }

    #[test]
    fn a_note_deleted_on_another_device_stops_showing_up_in_offline_search() {
        let cached = vec!["Kept.md".to_string(), "Deleted.md".to_string()];
        let keep = vec!["Kept.md".to_string()];

        let dropped = prune_targets(&cached, &keep, &[]);
        assert_eq!(dropped, vec!["Deleted.md".to_string()]);
    }

    #[test]
    fn a_note_created_offline_survives_a_tree_that_does_not_know_about_it_yet() {
        let cached = vec!["New.md".to_string()];
        let keep: Vec<String> = Vec::new();
        let queue = vec![queued(
            1,
            PendingOp::CreateNote {
                path: "New.md".to_string(),
                markdown: "# New".to_string(),
            },
        )];

        assert!(prune_targets(&cached, &keep, &queue).is_empty());
    }

    #[test]
    fn a_note_moved_offline_is_protected_at_its_new_path_not_its_old_one() {
        // The cache is renamed to the new path the moment the move is queued
        // (`vault::move_note`), so nothing is ever cached under `from` any more —
        // protecting it there would protect nothing.
        let cached = vec!["Projects/Budget.md".to_string()];
        let keep: Vec<String> = Vec::new();
        let queue = vec![queued(
            1,
            PendingOp::MoveNote {
                from: "Budget.md".to_string(),
                to: "Projects/Budget.md".to_string(),
            },
        )];

        assert!(prune_targets(&cached, &keep, &queue).is_empty());
    }

    #[test]
    fn a_note_inside_an_offline_folder_move_is_protected_too() {
        let cached = vec!["Archive/Projects/Reno.md".to_string()];
        let keep: Vec<String> = Vec::new();
        let queue = vec![queued(
            1,
            PendingOp::MoveFolder {
                from: "Projects".to_string(),
                to: "Archive/Projects".to_string(),
            },
        )];

        assert!(prune_targets(&cached, &keep, &queue).is_empty());
    }

    #[test]
    fn a_queued_delete_does_not_protect_anything() {
        let cached = vec!["Gone.md".to_string()];
        let keep: Vec<String> = Vec::new();
        let queue = vec![queued(
            1,
            PendingOp::DeleteNote {
                path: "Gone.md".to_string(),
            },
        )];

        assert_eq!(prune_targets(&cached, &keep, &queue), vec!["Gone.md".to_string()]);
    }
}
