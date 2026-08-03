//! Replaying what was done offline, once the server is there again.
//!
//! The queue is replayed **in order and one at a time**. That is slower than
//! firing everything at once and it is the only version that is correct:
//! creating `A.md` and then moving it to `Projects/A.md` is not the same thing
//! in the other order, and a rename that arrives before the file it renames
//! fails outright.
//!
//! Replay stops at the first conflict rather than working around it. A conflict
//! means two people — or one person on two machines — wrote different things
//! into the same note, and the only honest resolution is a decision by someone
//! who knows what the note is for. Everything queued behind it stays queued;
//! nothing is discarded, and nothing is overwritten. Resolving the conflict
//! restarts the replay from where it stopped.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::cache;
use super::queue::{PendingOp, QueuedOp};
use super::CachedNote;
use crate::api::{self, ApiFailure};
use crate::save;
use crate::state::{AppState, Conflict, ConflictOrigin, SyncPhase};

/// What replaying one operation did.
enum Outcome {
    /// The server has it now.
    Done,
    /// Two versions of the same note disagree; a person has to choose.
    Stop(Conflict),
    /// The server went away again mid-replay.
    Unreachable,
    /// The session expired. The queue is intact and will replay after signing
    /// back in.
    SignInNeeded,
    /// The server considered this and refused it — an invalid path, a note
    /// whose folder no longer exists. Retrying it unchanged will not help, so
    /// it is kept, marked, and shown to the user to discard or fix.
    Refused(String),
}

/// Starts a sync if one is not already running.
pub fn start(state: AppState) {
    if state.sync.get_untracked() == SyncPhase::Syncing {
        return;
    }
    spawn_local(async move { run(state).await });
}

/// Replays the outbox, then refreshes what is on screen.
pub async fn run(state: AppState) {
    if !state.online.get_untracked() {
        return;
    }
    // A conflict already on screen owns the queue until it is resolved.
    if !state.conflicts.get_untracked().is_empty() {
        state.sync.set(SyncPhase::Blocked);
        return;
    }

    let queue = cache::outbox().await;
    state.pending.set(queue.clone());
    if queue.is_empty() {
        state.sync.set(SyncPhase::Idle);
        state.sync_message.set(None);
        // Nothing of ours to replay does not mean nothing changed: this is
        // also how reconnecting and the "Sync now" button reach here, and
        // both are read-only refreshes as much as they are a replay.
        refresh_after_sync(state, &[]);
        return;
    }

    state.sync.set(SyncPhase::Syncing);
    state.sync_message.set(None);

    let mut applied = 0usize;
    let mut touched: Vec<String> = Vec::new();

    for queued in queue {
        // Something the user already looked at and could not be fixed
        // automatically; skip it rather than failing the whole queue on it.
        if queued.last_error.is_some() {
            continue;
        }

        match apply(state, &queued).await {
            Outcome::Done => {
                touched.push(queued.op.subject().to_string());
                state.pending.set(cache::drop_op(queued.id).await);
                applied += 1;
            }
            Outcome::Stop(conflict) => {
                state.push_conflict(conflict);
                state.sync.set(SyncPhase::Blocked);
                report(state, applied);
                return;
            }
            Outcome::Unreachable => {
                state.sync.set(SyncPhase::Idle);
                super::net::report_unreachable(state);
                report(state, applied);
                return;
            }
            Outcome::SignInNeeded => {
                state.sync.set(SyncPhase::Blocked);
                state.sync_message.set(Some(
                    "Your session expired. Sign in again and your local changes will sync."
                        .to_string(),
                ));
                report(state, applied);
                return;
            }
            Outcome::Refused(reason) => {
                state.pending.set(cache::mark_failed(queued.id, &reason).await);
            }
        }
    }

    let failed = state
        .pending
        .get_untracked()
        .iter()
        .filter(|queued| queued.last_error.is_some())
        .count();

    state.sync.set(if failed > 0 {
        SyncPhase::Blocked
    } else {
        SyncPhase::Idle
    });

    report(state, applied);
    refresh_after_sync(state, &touched);
}

/// Tells the user what just happened, once, rather than per operation.
fn report(state: AppState, applied: usize) {
    if applied == 0 {
        return;
    }
    let plural = if applied == 1 { "change" } else { "changes" };
    state.notify(format!("Synced {applied} {plural} made offline."));
}

/// Refetches what the replay may have changed on the server.
fn refresh_after_sync(state: AppState, touched: &[String]) {
    state.refresh_all();

    // Reload the open note only when the user is not mid-sentence in it: the
    // text they are typing is newer than anything a reload could bring back.
    let active = state.active_path();
    let dirty = state
        .tabs
        .get_untracked()
        .iter()
        .any(|tab| Some(&tab.path) == active.as_ref() && tab.dirty);

    if let Some(path) = active {
        if !dirty && touched.iter().any(|changed| *changed == path) {
            state.request_reload();
        }
    }
}

/// Sends one queued operation.
///
/// Takes the state because a successful write returns a *new* content hash, and
/// an open tab holding the old one would send it as the next `If-Match`. For a
/// note created offline that hash is the empty string, so the very next save
/// after a sync would come back as a conflict the user never caused.
async fn apply(state: AppState, queued: &QueuedOp) -> Outcome {
    match &queued.op {
        PendingOp::CreateNote { path, markdown } => {
            match api::create_note(path.clone(), markdown.clone()).await {
                Ok(response) => {
                    cache::put_note(&CachedNote::new(
                        response.meta.path.clone(),
                        markdown.clone(),
                        response.meta.content_hash.clone(),
                    ))
                    .await;
                    state.set_hash(&response.meta.path, response.meta.content_hash);
                    Outcome::Done
                }
                // The note grew on the server while we were away — from another
                // device, or from an editor over SSH. Same disagreement as a
                // failed save, and the same three ways out.
                Err(ApiFailure::AlreadyExists(_)) => {
                    match api::read_note(path.clone()).await {
                        // The same text on both sides is not a conflict; it is
                        // the note we were trying to create, already there —
                        // an earlier replay that landed before its
                        // acknowledgement got back, most likely.
                        Ok(theirs) if theirs.markdown == *markdown => {
                            cache::put_note(&CachedNote::new(
                                path.clone(),
                                markdown.clone(),
                                theirs.meta.content_hash.clone(),
                            ))
                            .await;
                            state.set_hash(path, theirs.meta.content_hash);
                            Outcome::Done
                        }
                        Ok(theirs) => Outcome::Stop(Conflict {
                            path: path.clone(),
                            mine: markdown.clone(),
                            theirs: theirs.markdown,
                            their_hash: theirs.meta.content_hash,
                            origin: ConflictOrigin::Sync { op_id: queued.id },
                        }),
                        Err(err) => classify(err),
                    }
                }
                Err(err) => classify(err),
            }
        }

        PendingOp::SaveNote {
            path,
            markdown,
            base_hash,
        } => match api::save_note(path.clone(), markdown.clone(), base_hash.clone()).await {
            Ok(response) => {
                cache::put_note(&CachedNote::new(
                    path.clone(),
                    markdown.clone(),
                    response.meta.content_hash.clone(),
                ))
                .await;
                state.set_hash(path, response.meta.content_hash);
                Outcome::Done
            }
            // The same text on both sides is not a conflict; it is this
            // device's own write, already applied by an earlier replay that
            // landed before its acknowledgement got back.
            Err(ApiFailure::Conflict(body)) if body.current_markdown == *markdown => {
                cache::put_note(&CachedNote::new(
                    path.clone(),
                    markdown.clone(),
                    body.current_hash.clone(),
                ))
                .await;
                state.set_hash(path, body.current_hash);
                Outcome::Done
            }
            Err(ApiFailure::Conflict(body)) => Outcome::Stop(Conflict {
                path: path.clone(),
                mine: markdown.clone(),
                theirs: body.current_markdown,
                their_hash: body.current_hash,
                origin: ConflictOrigin::Sync { op_id: queued.id },
            }),
            // The note was deleted on the server while we were editing it here.
            // Recreating it is the choice that keeps the writing.
            Err(ApiFailure::NotFound) => {
                match api::create_note(path.clone(), markdown.clone()).await {
                    Ok(response) => {
                        cache::put_note(&CachedNote::new(
                            path.clone(),
                            markdown.clone(),
                            response.meta.content_hash.clone(),
                        ))
                        .await;
                        state.set_hash(path, response.meta.content_hash);
                        Outcome::Done
                    }
                    Err(err) => classify(err),
                }
            }
            Err(err) => classify(err),
        },

        PendingOp::DeleteNote { path } => match api::delete_note(path.clone()).await {
            // Already gone is the outcome we wanted.
            Ok(()) | Err(ApiFailure::NotFound) => {
                cache::remove_note(path).await;
                Outcome::Done
            }
            Err(err) => classify(err),
        },

        PendingOp::MoveNote { from, to } => match api::move_note(from.clone(), to.clone()).await {
            Ok(_) => {
                cache::rename_notes(from, to).await;
                Outcome::Done
            }
            Err(err) => classify(err),
        },

        PendingOp::CreateFolder { path } => match api::create_folder(path.clone()).await {
            Ok(()) | Err(ApiFailure::AlreadyExists(_)) => Outcome::Done,
            Err(err) => classify(err),
        },

        PendingOp::MoveFolder { from, to } => {
            match api::move_folder(from.clone(), to.clone()).await {
                Ok(_) => {
                    cache::rename_notes(from, to).await;
                    Outcome::Done
                }
                Err(err) => classify(err),
            }
        }

        PendingOp::DeleteFolder { path } => match api::delete_folder(path.clone()).await {
            Ok(()) | Err(ApiFailure::NotFound) => {
                cache::remove_under(path).await;
                Outcome::Done
            }
            Err(err) => classify(err),
        },
    }
}

fn classify(err: ApiFailure) -> Outcome {
    match err {
        ApiFailure::Offline(_) => Outcome::Unreachable,
        ApiFailure::Unauthenticated => Outcome::SignInNeeded,
        other => Outcome::Refused(other.user_message()),
    }
}

// ---------------------------------------------------------------------------
// Resolving a conflict
// ---------------------------------------------------------------------------

/// Keeps the local version, saved against the hash the server just reported so
/// the write is accepted rather than rejected a second time.
///
/// Saves whatever is actually in the editor right now, not the snapshot the
/// conflict was raised with — the dialog does not stop someone from typing
/// while it decides what to show them, and "keep mine" should keep that too.
pub fn keep_mine(state: AppState, conflict: Conflict) {
    state.clear_conflict(&conflict.path);

    let mine = if state.active_path().as_deref() == Some(conflict.path.as_str()) {
        state.active_markdown.get_untracked()
    } else {
        conflict.mine.clone()
    };

    spawn_local(async move {
        match api::save_note(conflict.path.clone(), mine.clone(), conflict.their_hash.clone()).await {
            Ok(response) => {
                cache::put_note(&CachedNote::new(
                    conflict.path.clone(),
                    mine.clone(),
                    response.meta.content_hash.clone(),
                ))
                .await;
                save::resolved_kept(state, conflict.path.clone(), response.meta.content_hash.clone(), mine);
                settle(state, &conflict).await;
                state.notify("Kept your version.");
                state.refresh_all();
            }
            Err(err) => fail(state, err, "Your version could not be saved"),
        }
    });
}

/// Takes the server's version, discarding the local one — including anything
/// typed since the conflict was raised, which is what this choice means.
pub fn take_theirs(state: AppState, conflict: Conflict) {
    state.clear_conflict(&conflict.path);

    spawn_local(async move {
        cache::put_note(&CachedNote::new(
            conflict.path.clone(),
            conflict.theirs.clone(),
            conflict.their_hash.clone(),
        ))
        .await;
        save::resolved_replaced(
            state,
            conflict.path.clone(),
            conflict.their_hash.clone(),
            conflict.theirs.clone(),
        );
        settle(state, &conflict).await;

        if state.active_path().as_deref() == Some(conflict.path.as_str()) {
            state.request_reload();
        }
        state.notify("Loaded the version from the server.");
        state.refresh_all();
    });
}

/// Keeps both: the local version is saved alongside as a separate note, and the
/// server's version stays where it is.
///
/// This is the option that never loses anybody's writing, which is why it is
/// offered even though it leaves the vault with two files to merge by hand.
pub fn keep_both(state: AppState, conflict: Conflict) {
    state.clear_conflict(&conflict.path);

    spawn_local(async move {
        let copy = conflicted_copy_path(&conflict.path);
        match api::create_note(copy.clone(), conflict.mine.clone()).await {
            Ok(response) => {
                cache::put_note(&CachedNote::new(
                    response.meta.path.clone(),
                    conflict.mine.clone(),
                    response.meta.content_hash.clone(),
                ))
                .await;
                cache::put_note(&CachedNote::new(
                    conflict.path.clone(),
                    conflict.theirs.clone(),
                    conflict.their_hash.clone(),
                ))
                .await;
                save::resolved_replaced(
                    state,
                    conflict.path.clone(),
                    conflict.their_hash.clone(),
                    conflict.theirs.clone(),
                );
                settle(state, &conflict).await;

                if state.active_path().as_deref() == Some(conflict.path.as_str()) {
                    state.request_reload();
                }
                state.refresh_all();
                state.open_tab(response.meta.path.clone(), response.meta.title.clone());
                save::opened(
                    state,
                    &response.meta.path,
                    response.meta.content_hash.clone(),
                    conflict.mine.clone(),
                );
                state.notify("Saved your version as a separate note.");
            }
            Err(err) => fail(state, err, "The copy could not be saved"),
        }
    });
}

/// Clears the queued operation a conflict came from, then carries on with the
/// rest of the queue.
async fn settle(state: AppState, conflict: &Conflict) {
    if let ConflictOrigin::Sync { op_id } = conflict.origin {
        state.pending.set(cache::drop_op(op_id).await);
    }
    if state.conflicts.get_untracked().is_empty() {
        state.sync.set(SyncPhase::Idle);
        start(state);
    }
}

fn fail(state: AppState, err: ApiFailure, prefix: &str) {
    if err.is_offline() {
        super::net::report_unreachable(state);
        state.error(format!("{prefix}: the server is not reachable right now."));
        return;
    }
    state.error(format!("{prefix}: {}", err.user_message()));
}

/// `Projects/Budget.md` → `Projects/Budget (conflicted copy 2026-08-01T09-15-22).md`
fn conflicted_copy_path(path: &str) -> String {
    let stem = go_notes_shared::paths::stem(path);
    let parent = go_notes_shared::paths::parent_of(path);
    let stamp: String = chrono::Utc::now()
        .to_rfc3339()
        .chars()
        .take(19)
        .map(|c| if c == ':' { '-' } else { c })
        .collect();
    go_notes_shared::paths::join(parent, &format!("{stem} (conflicted copy {stamp}).md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflicted_copy_sits_beside_the_original() {
        let copy = conflicted_copy_path("Projects/Budget.md");
        assert!(copy.starts_with("Projects/Budget (conflicted copy "));
        assert!(copy.ends_with(".md"));
        // No colons: they are legal on Linux and refused on Windows, and the
        // whole point of a vault of plain files is that it travels.
        assert!(!copy.contains(':'));
    }

    #[test]
    fn a_conflicted_copy_of_a_root_note_stays_at_the_root() {
        let copy = conflicted_copy_path("Budget.md");
        assert!(!copy.contains('/'));
    }
}
