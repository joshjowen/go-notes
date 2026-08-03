//! Serialises writes to each note, so overlapping autosaves stop fighting.
//!
//! Autosave fires 800ms after the last keystroke, but nothing used to stop a
//! second save from going out before the first one's response — and, more
//! often than it looked like, before the browser had even finished the
//! *previous* request behind it: typing a link and then moving the caret
//! around fires a burst of other requests (wikilink lookups, backlinks) that
//! queue ahead of the save on the browser's own connection limit. The second
//! save would then carry the token the *first* one had already invalidated,
//! the server would refuse it, and the conflict dialog would show the user
//! their own text from a moment ago as if it were someone else's.
//!
//! Everything here keys off the note's path, in a `thread_local!` map rather
//! than component state, because the debounce timer and Ctrl+S both need to
//! reach the same in-flight bookkeping from outside any one component.
//!
//! A conflict that survives the checks below is a real one — the vault's
//! actual [`super::offline::sync`] queue still keeps stopping at the first
//! disagreement and asking a person, exactly as before. Nothing here picks a
//! winner between two edits to the same line.

use std::cell::RefCell;
use std::collections::HashMap;

use go_notes_shared::ConflictBody;
use leptos::task::spawn_local;

use crate::api::ApiFailure;
use crate::offline::merge::{self, Merge};
use crate::state::{AppState, Conflict, ConflictOrigin};
use crate::vault;

/// Above this many automatic retries in a row, stop trying to resolve a
/// conflict alone and ask a person — a safety valve against a save that keeps
/// losing a race with itself, not something ordinary use should ever reach.
const MAX_AUTO_RETRIES: u32 = 5;

#[derive(Default)]
struct Slot {
    in_flight: bool,
    /// Typed while a save was outstanding — the next text to send, once the
    /// one in flight is settled.
    queued: Option<String>,
    /// The text the server confirmed for the tab's current hash: the common
    /// ancestor a conflict is merged against.
    base: Option<String>,
    /// A conflict for this path is on screen; autosave holds off until it is
    /// answered rather than resending the same stale token every debounce.
    blocked: bool,
}

thread_local! {
    static SLOTS: RefCell<HashMap<String, Slot>> = RefCell::new(HashMap::new());
}

fn with_slot<T>(path: &str, f: impl FnOnce(&mut Slot) -> T) -> T {
    SLOTS.with(|slots| {
        let mut slots = slots.borrow_mut();
        f(slots.entry(path.to_string()).or_default())
    })
}

/// Records what the server confirmed for a note just opened or created, so
/// the first save against it carries a real token instead of an empty one,
/// and a later conflict has a base text to merge from.
pub fn opened(state: AppState, path: &str, hash: String, markdown: String) {
    state.set_hash(path, hash);
    with_slot(path, |slot| {
        slot.base = Some(markdown);
        slot.blocked = false;
    });
}

/// The debounce and Ctrl+S entry point.
///
/// If a save for this path is already in flight, or a conflict for it is
/// still on screen, the text is remembered rather than sent immediately — the
/// outstanding request's answer, or the person's decision, comes first.
pub fn request(state: AppState, path: String, markdown: String) {
    let should_run = with_slot(&path, |slot| {
        if slot.blocked || slot.in_flight {
            slot.queued = Some(markdown.clone());
            false
        } else {
            slot.in_flight = true;
            true
        }
    });
    if should_run {
        spawn_local(run(state, path, markdown));
    }
}

/// Writes immediately rather than waiting for the next debounce.
///
/// Used when switching away from a note, so the edit made in the last 800ms
/// is sent rather than simply dropped along with the pending timer.
pub fn flush(state: AppState, path: String, markdown: String) {
    request(state, path, markdown);
}

/// Called when a conflict is resolved by keeping this device's writing:
/// clears the block, adopts the hash the resolution just got, and sends
/// whatever was typed while the dialog was open — the choice this button
/// makes is to keep everything written here, not just the snapshot the
/// conflict happened to be raised with.
pub fn resolved_kept(state: AppState, path: String, hash: String, markdown: String) {
    state.set_hash(&path, hash);
    state.mark_dirty(&path, false);
    let queued = with_slot(&path, |slot| {
        slot.blocked = false;
        slot.base = Some(markdown);
        slot.queued.take()
    });
    if let Some(pending) = queued {
        request(state, path, pending);
    }
}

/// Called when a conflict is resolved by taking the server's version instead:
/// clears the block and adopts the server's text as the new base, discarding
/// anything typed while the dialog was open along with the rest of the local
/// edit — that is what choosing the server's version means.
pub fn resolved_replaced(state: AppState, path: String, hash: String, markdown: String) {
    state.set_hash(&path, hash);
    state.mark_dirty(&path, false);
    with_slot(&path, |slot| {
        slot.blocked = false;
        slot.base = Some(markdown);
        slot.queued = None;
    });
}

/// What one save attempt produced, once a conflict (if any) has been
/// classified.
enum Attempt {
    Saved {
        hash: String,
        /// True when the server was not reachable and the write only joined
        /// the offline outbox — nothing changed there yet, so there is
        /// nothing new to refetch.
        queued: bool,
    },
    /// A conflict resolved itself: adopt `hash`, and if `resend` holds text,
    /// send it before considering this settled — the server does not have it
    /// yet, either because it was never sent (a merge) or because the token
    /// alone was stale (an echo of our own prior write, or of an unrelated
    /// race that never touched the content).
    Settled {
        hash: String,
        resend: Option<String>,
    },
    /// A real disagreement. Left for a person.
    Conflict(Conflict),
    /// Offline, refused, or anything else — already reported to the user.
    Failed,
}

async fn attempt_save(state: AppState, path: &str, markdown: &str, expected: String) -> Attempt {
    match vault::save_note(state, path.to_string(), markdown.to_string(), expected).await {
        Ok(written) => Attempt::Saved {
            hash: written.content_hash,
            queued: written.queued,
        },
        Err(ApiFailure::Conflict(body)) => classify_conflict(path, markdown, body),
        Err(err) => {
            state.error(err.user_message());
            Attempt::Failed
        }
    }
}

/// Decides whether a 409 is really a disagreement, or something this device
/// can settle on its own.
fn classify_conflict(path: &str, mine: &str, body: ConflictBody) -> Attempt {
    let theirs = body.current_markdown;
    let hash = body.current_hash;

    // Our own write already landed and this device merely lost the
    // acknowledgement — the case behind the reported bug, where two
    // overlapping autosaves for the same edit raced each other.
    if theirs == mine {
        return Attempt::Settled { hash, resend: None };
    }

    let base = with_slot(path, |slot| slot.base.clone());

    // The token this device sent was stale, but the server's content has not
    // actually moved since the last save this device knows about — safe to
    // resend the same text against the corrected token.
    if base.as_deref() == Some(theirs.as_str()) {
        return Attempt::Settled {
            hash,
            resend: Some(mine.to_string()),
        };
    }

    // A real edit landed elsewhere. If it touched a different part of the
    // note than this edit did, both can be kept without asking.
    if let Some(base) = &base {
        if let Merge::Clean(merged) = merge::three_way(base, mine, &theirs) {
            return Attempt::Settled {
                hash,
                resend: Some(merged),
            };
        }
    }

    Attempt::Conflict(Conflict {
        path: path.to_string(),
        theirs,
        their_hash: hash,
        mine: mine.to_string(),
        origin: ConflictOrigin::Live,
    })
}

async fn run(state: AppState, path: String, markdown: String) {
    let mut expected = resolve_hash(state, &path).await;
    let mut current = markdown;
    let mut wrote = false;
    let mut retries = 0u32;

    loop {
        let attempt = attempt_save(state, &path, &current, expected.clone()).await;
        match attempt {
            Attempt::Saved { hash, queued } => {
                with_slot(&path, |slot| slot.base = Some(current.clone()));
                state.set_hash(&path, hash);
                state.mark_dirty(&path, false);
                // A write that only reached the outbox has not changed
                // anything the server would answer differently about yet.
                wrote = wrote || !queued;
            }
            Attempt::Settled { hash, resend: None } => {
                with_slot(&path, |slot| slot.base = Some(current.clone()));
                state.set_hash(&path, hash);
                state.mark_dirty(&path, false);
                wrote = true;
            }
            Attempt::Settled {
                hash,
                resend: Some(next),
            } => {
                retries += 1;
                if retries > MAX_AUTO_RETRIES {
                    with_slot(&path, |slot| slot.blocked = true);
                    state.push_conflict(Conflict {
                        path: path.clone(),
                        theirs: next,
                        their_hash: hash,
                        mine: current,
                        origin: ConflictOrigin::Live,
                    });
                    break;
                }
                // Not a new edit, so it bypasses the queue and retries at once.
                expected = hash;
                current = next;
                continue;
            }
            Attempt::Conflict(conflict) => {
                with_slot(&path, |slot| slot.blocked = true);
                state.push_conflict(conflict);
                break;
            }
            Attempt::Failed => break,
        }

        match with_slot(&path, |slot| slot.queued.take()) {
            Some(next) => {
                current = next;
                expected = state.hash_for(&path);
                retries = 0;
            }
            None => break,
        }
    }

    with_slot(&path, |slot| slot.in_flight = false);
    // Once per debounce's worth of writes, not once per write: a save can
    // create links, so the tree and graph may both have changed, but nothing
    // here needs refetching them more than once a burst actually settles.
    if wrote {
        state.refresh_all();
    }
}

/// Gets a real `If-Match` token when the tab does not have one yet.
///
/// An empty hash almost always means a save landed before the note finished
/// loading — `save::request` is only ever reached for a note already open, so
/// the note exists on the server and a re-read gets its current token rather
/// than sending an empty one, which an existing file always refuses.
async fn resolve_hash(state: AppState, path: &str) -> String {
    let expected = state.hash_for(path);
    if !expected.is_empty() {
        return expected;
    }
    match vault::read_note(state, path.to_string()).await {
        Ok(note) => note.meta.content_hash,
        Err(_) => expected,
    }
}
