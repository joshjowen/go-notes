//! Local-first access to the vault.
//!
//! Every part of the interface that reads or writes notes goes through here
//! rather than calling [`crate::api`] directly, because every one of those
//! operations has to have an answer when the server is not there. The shape is
//! always the same:
//!
//! * Ask the server. If it answers, write what came back into the local copy,
//!   so the next disconnection starts from something current.
//! * If the request never reached the server — and *only* then — fall back to
//!   the local copy, and record any change in the outbox for replay.
//!
//! The distinction in that second point is the important one. A request the
//! server refused (an invalid path, a note that is already there) is a real
//! error and is reported as one; a request that never arrived is not the user's
//! problem and must not interrupt them.

use go_notes_shared::{
    Backlink, NoteMeta, NoteResponse, QuickSwitchItem, SearchHit, TagCount, TreeNode,
};
use leptos::prelude::*;

use crate::api::{self, ApiFailure, ApiResult};
use crate::offline::queue::PendingOp;
use crate::offline::{cache, index, net, sync, tree as local_tree, CachedNote};
use crate::state::AppState;

/// What happened to a write.
#[derive(Debug, Clone)]
pub struct Written {
    pub path: String,
    pub title: String,
    /// The hash to send with the next save. For a queued write this is still
    /// the server's last known hash — the token the eventual replay needs.
    pub content_hash: String,
    /// True when this is recorded on the device rather than on the server.
    pub queued: bool,
}

/// Notes reachability from a completed request.
fn observe<T>(state: AppState, result: ApiResult<T>) -> ApiResult<T> {
    match &result {
        Err(ApiFailure::Offline(reason)) => {
            // The transport's own words, in the console rather than in front of
            // the user: "TypeError: Failed to fetch" helps whoever is debugging
            // a proxy and means nothing to anybody else.
            web_sys::console::debug_1(&wasm_bindgen::JsValue::from_str(&format!(
                "go-notes: {reason}"
            )));
            net::report_unreachable(state);
        }
        _ => net::report_reachable(state),
    }
    result
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The file tree, from the server if it answers and from the local copy if not.
pub async fn tree(state: AppState) -> ApiResult<TreeNode> {
    match observe(state, api::tree().await) {
        Ok(tree) => {
            cache::put_tree(&tree).await;
            // Only on a successful fetch: the server's tree is the authority on
            // what still exists, and pruning against a cached fallback would
            // compare a stale list against itself.
            cache::retain_notes(&local_tree::note_paths(&tree)).await;
            Ok(tree)
        }
        Err(err) if err.is_offline() => match cache::tree().await {
            Some(tree) => Ok(tree),
            None => Err(err),
        },
        Err(err) => Err(err),
    }
}

/// A note, with its backlinks.
///
/// Offline, the backlinks are derived from the notes this device holds, so they
/// are a subset of the real ones — a note that has never been opened here
/// cannot be known to link anywhere. That is visible in the pane rather than
/// pretended away.
pub async fn read_note(state: AppState, path: String) -> ApiResult<NoteResponse> {
    match observe(state, api::read_note(path.clone()).await) {
        Ok(note) => {
            cache::put_note(&CachedNote::new(
                note.meta.path.clone(),
                note.markdown.clone(),
                note.meta.content_hash.clone(),
            ))
            .await;
            Ok(note)
        }
        Err(err) if err.is_offline() => match cache::note(&path).await {
            Some(cached) => Ok(local_note(cached, cache::all_notes().await)),
            None => Err(ApiFailure::Message(format!(
                "'{path}' has not been opened on this device, so there is no local copy to show."
            ))),
        },
        Err(err) => Err(err),
    }
}

fn local_note(cached: CachedNote, all: Vec<CachedNote>) -> NoteResponse {
    NoteResponse {
        meta: NoteMeta {
            path: cached.path.clone(),
            title: cached.title.clone(),
            content_hash: cached.content_hash.clone(),
            modified: cached.updated_at,
            size_bytes: cached.markdown.len() as i64,
            tags: index::tags_in(&cached.markdown),
        },
        backlinks: index::backlinks(&all, &cached.path),
        // Outgoing links are only used by the server's own graph building; the
        // interface reads backlinks. Left empty rather than half-computed.
        outgoing: Vec::new(),
        // No local equivalent of the embeddings model, so nothing to suggest
        // from a cached copy.
        suggested: Vec::new(),
        markdown: cached.markdown,
    }
}

pub async fn backlinks(state: AppState, path: String) -> Vec<Backlink> {
    match observe(state, api::backlinks(path.clone()).await) {
        Ok(links) => links,
        Err(err) if err.is_offline() => index::backlinks(&cache::all_notes().await, &path),
        Err(_) => Vec::new(),
    }
}

pub async fn search(state: AppState, query: String) -> ApiResult<Vec<SearchHit>> {
    match observe(state, api::search(query.clone()).await) {
        Ok(response) => Ok(response.hits),
        Err(err) if err.is_offline() => Ok(index::search(&cache::all_notes().await, &query)),
        Err(err) => Err(err),
    }
}

pub async fn quickswitch(state: AppState, query: String) -> Vec<QuickSwitchItem> {
    match observe(state, api::quickswitch(query.clone()).await) {
        Ok(items) => items,
        Err(err) if err.is_offline() => index::quickswitch(&cache::all_notes().await, &query),
        Err(_) => Vec::new(),
    }
}

pub async fn tags(state: AppState) -> Vec<TagCount> {
    match observe(state, api::tags().await) {
        Ok(found) => found,
        Err(err) if err.is_offline() => index::tags(&cache::all_notes().await),
        Err(_) => Vec::new(),
    }
}

pub async fn notes_with_tag(state: AppState, tag: String) -> Vec<QuickSwitchItem> {
    match observe(state, api::notes_with_tag(tag.clone()).await) {
        Ok(items) => items,
        Err(err) if err.is_offline() => index::notes_with_tag(&cache::all_notes().await, &tag),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

/// Saves a note, queueing the write if the server cannot be reached.
///
/// A conflict is passed straight back to the caller: it is a decision for a
/// person, and this layer must not make it for them.
pub async fn save_note(
    state: AppState,
    path: String,
    markdown: String,
    expected_hash: String,
) -> ApiResult<Written> {
    let result = api::save_note(path.clone(), markdown.clone(), expected_hash.clone()).await;

    match observe(state, result) {
        Ok(response) => {
            cache::put_note(&CachedNote::new(
                path.clone(),
                markdown,
                response.meta.content_hash.clone(),
            ))
            .await;
            Ok(Written {
                path: response.meta.path,
                title: response.meta.title,
                content_hash: response.meta.content_hash,
                queued: false,
            })
        }
        Err(err) if err.is_offline() => {
            cache::put_note(&CachedNote::new(
                path.clone(),
                markdown.clone(),
                expected_hash.clone(),
            ))
            .await;
            enqueue(
                state,
                PendingOp::SaveNote {
                    path: path.clone(),
                    markdown,
                    // The server's hash, not one computed here: this is the
                    // token the replay will send as `If-Match`, and it has to be
                    // the version the server actually holds.
                    base_hash: expected_hash.clone(),
                },
            )
            .await;

            Ok(Written {
                title: crate::state::title_of(&path),
                path,
                content_hash: expected_hash,
                queued: true,
            })
        }
        Err(err) => Err(err),
    }
}

pub async fn create_note(state: AppState, path: String, markdown: String) -> ApiResult<Written> {
    match observe(state, api::create_note(path.clone(), markdown.clone()).await) {
        Ok(response) => {
            cache::put_note(&CachedNote::new(
                response.meta.path.clone(),
                markdown,
                response.meta.content_hash.clone(),
            ))
            .await;
            Ok(Written {
                path: response.meta.path,
                title: response.meta.title,
                content_hash: response.meta.content_hash,
                queued: false,
            })
        }
        Err(err) if err.is_offline() => {
            // A note that exists only here has no server hash yet, and an empty
            // `If-Match` is exactly what the server expects for a file that is
            // not there.
            cache::put_note(&CachedNote::new(
                path.clone(),
                markdown.clone(),
                String::new(),
            ))
            .await;
            update_tree(state, |tree| local_tree::insert_note(tree, &path)).await;
            enqueue(
                state,
                PendingOp::CreateNote {
                    path: path.clone(),
                    markdown,
                },
            )
            .await;

            Ok(Written {
                title: crate::state::title_of(&path),
                path,
                content_hash: String::new(),
                queued: true,
            })
        }
        Err(err) => Err(err),
    }
}

pub async fn delete_note(state: AppState, path: String) -> ApiResult<bool> {
    match observe(state, api::delete_note(path.clone()).await) {
        Ok(()) => {
            cache::remove_note(&path).await;
            Ok(false)
        }
        Err(err) if err.is_offline() => {
            cache::remove_note(&path).await;
            update_tree(state, |tree| local_tree::remove(tree, &path)).await;
            enqueue(state, PendingOp::DeleteNote { path }).await;
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

/// Moves a note. The returned count is how many other notes had links rewritten
/// — always zero for a queued move, because the rewrite happens on the server
/// when the move is replayed.
pub async fn move_note(state: AppState, from: String, to: String) -> ApiResult<(String, usize)> {
    match observe(state, api::move_note(from.clone(), to.clone()).await) {
        Ok(response) => {
            cache::rename_notes(&from, &response.to).await;
            Ok((response.to, response.links_rewritten))
        }
        Err(err) if err.is_offline() => {
            cache::rename_notes(&from, &to).await;
            update_tree(state, |tree| local_tree::rename(tree, &from, &to)).await;
            enqueue(
                state,
                PendingOp::MoveNote {
                    from,
                    to: to.clone(),
                },
            )
            .await;
            Ok((to, 0))
        }
        Err(err) => Err(err),
    }
}

pub async fn move_folder(state: AppState, from: String, to: String) -> ApiResult<(String, usize)> {
    match observe(state, api::move_folder(from.clone(), to.clone()).await) {
        Ok(response) => {
            cache::rename_notes(&from, &response.to).await;
            Ok((response.to, response.links_rewritten))
        }
        Err(err) if err.is_offline() => {
            cache::rename_notes(&from, &to).await;
            update_tree(state, |tree| local_tree::rename(tree, &from, &to)).await;
            enqueue(
                state,
                PendingOp::MoveFolder {
                    from,
                    to: to.clone(),
                },
            )
            .await;
            Ok((to, 0))
        }
        Err(err) => Err(err),
    }
}

pub async fn create_folder(state: AppState, path: String) -> ApiResult<bool> {
    match observe(state, api::create_folder(path.clone()).await) {
        Ok(()) => Ok(false),
        Err(err) if err.is_offline() => {
            update_tree(state, |tree| local_tree::insert_folder(tree, &path)).await;
            enqueue(state, PendingOp::CreateFolder { path }).await;
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

pub async fn delete_folder(state: AppState, path: String) -> ApiResult<bool> {
    match observe(state, api::delete_folder(path.clone()).await) {
        Ok(()) => {
            cache::remove_under(&path).await;
            Ok(false)
        }
        Err(err) if err.is_offline() => {
            cache::remove_under(&path).await;
            update_tree(state, |tree| local_tree::remove(tree, &path)).await;
            enqueue(state, PendingOp::DeleteFolder { path }).await;
            Ok(true)
        }
        Err(err) => Err(err),
    }
}

/// Whether a folder is collapsed in the sidebar. Per-user interface state
/// rather than a change to the vault, so it is never queued — it is simply
/// remembered locally until the server can be told again.
pub async fn set_folder_collapsed(state: AppState, path: String, collapsed: bool) {
    if observe(state, api::set_folder_collapsed(path.clone(), collapsed).await).is_err() {
        update_tree(state, |tree| {
            local_tree::set_collapsed(tree, &path, collapsed)
        })
        .await;
    }
}

/// Uploads an attachment.
///
/// This is the one operation with no offline answer, and it says so plainly
/// rather than pretending. Queueing the bytes would mean inserting a link into
/// the note that resolves to nothing until a sync that might be days away, and
/// then rewriting the note's text underneath the author to correct the path —
/// editing someone's writing behind their back to cover for a feature that was
/// not available.
pub async fn upload_attachment(
    state: AppState,
    file: web_sys::File,
) -> ApiResult<go_notes_shared::AttachmentResponse> {
    match observe(state, api::upload_attachment(file).await) {
        Ok(response) => Ok(response),
        Err(err) if err.is_offline() => Err(ApiFailure::Message(
            "Attachments need a connection to the server. This one was not added.".into(),
        )),
        Err(err) => Err(err),
    }
}

// ---------------------------------------------------------------------------
// Local bookkeeping
// ---------------------------------------------------------------------------

/// Records a change for replay and updates the pending count on screen.
async fn enqueue(state: AppState, op: PendingOp) {
    state.pending.set(cache::enqueue(op).await);
}

/// Applies a change to the cached tree and shows it immediately.
///
/// Without this the sidebar would not react at all offline: a new note would be
/// invisible until the next successful `/api/tree`, which may be tomorrow.
async fn update_tree(state: AppState, edit: impl FnOnce(&mut TreeNode)) {
    let Some(mut tree) = cache::tree().await else {
        return;
    };
    edit(&mut tree);
    cache::put_tree(&tree).await;
    state.tree.set(Some(tree));
}

/// Re-runs the sync, for the "Sync now" button.
pub fn sync_now(state: AppState) {
    if state.online.get_untracked() {
        sync::start(state);
        return;
    }
    net::report_unreachable(state);
}
