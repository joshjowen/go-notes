//! The outbox: work the browser owes the server.
//!
//! Everything the user does while the server is unreachable is recorded here as
//! an operation to replay later. The queue is ordered and replayed in order,
//! because the operations are not independent — creating `A.md` and then moving
//! it to `Projects/A.md` only makes sense in that sequence.
//!
//! Two rules shape the whole design:
//!
//! * **A save carries the hash the *server* last gave us**, never one computed
//!   locally. That hash is the `If-Match` token, and it is what turns "the file
//!   changed on disk while you were away" into a conflict the user resolves
//!   rather than into a silent overwrite of somebody else's work.
//! * **The queue is compacted, not appended to forever.** Autosave fires every
//!   time typing pauses, so an afternoon offline would otherwise queue hundreds
//!   of copies of the same note. [`coalesce`] collapses them, keeping the first
//!   base hash and the last text.

use go_notes_shared::paths;
use serde::{Deserialize, Serialize};

/// One replayable change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PendingOp {
    CreateNote {
        path: String,
        markdown: String,
    },
    SaveNote {
        path: String,
        markdown: String,
        /// The content hash the server last confirmed, or empty for a note that
        /// only exists on this device.
        base_hash: String,
    },
    DeleteNote {
        path: String,
    },
    MoveNote {
        from: String,
        to: String,
    },
    CreateFolder {
        path: String,
    },
    MoveFolder {
        from: String,
        to: String,
    },
    DeleteFolder {
        path: String,
    },
}

impl PendingOp {
    /// Whether this operation changes what lives at `path`.
    ///
    /// Folder operations count for anything underneath them: moving `Projects`
    /// moves `Projects/Budget.md` with it, so a queued save for that note must
    /// not be merged across the move.
    pub fn touches(&self, path: &str) -> bool {
        match self {
            PendingOp::CreateNote { path: at, .. }
            | PendingOp::SaveNote { path: at, .. }
            | PendingOp::DeleteNote { path: at }
            | PendingOp::CreateFolder { path: at } => at == path,
            PendingOp::DeleteFolder { path: at } => at == path || paths::is_within(path, at),
            PendingOp::MoveNote { from, to } => from == path || to == path,
            PendingOp::MoveFolder { from, to } => {
                from == path
                    || to == path
                    || paths::is_within(path, from)
                    || paths::is_within(path, to)
            }
        }
    }

    /// True when the operation changes *which file* a path refers to, rather
    /// than only its contents. Compaction never reaches across one of these.
    fn changes_identity(&self) -> bool {
        matches!(
            self,
            PendingOp::MoveNote { .. } | PendingOp::MoveFolder { .. } | PendingOp::DeleteFolder { .. }
        )
    }

    /// The path a human would say this operation is "about", for the status list.
    pub fn subject(&self) -> &str {
        match self {
            PendingOp::CreateNote { path, .. }
            | PendingOp::SaveNote { path, .. }
            | PendingOp::DeleteNote { path }
            | PendingOp::CreateFolder { path }
            | PendingOp::DeleteFolder { path } => path,
            PendingOp::MoveNote { from, .. } | PendingOp::MoveFolder { from, .. } => from,
        }
    }

    /// One line for the sync popover, in the user's terms rather than the API's.
    pub fn describe(&self) -> String {
        match self {
            PendingOp::CreateNote { path, .. } => format!("New note — {path}"),
            PendingOp::SaveNote { path, .. } => format!("Edit — {path}"),
            PendingOp::DeleteNote { path } => format!("Delete — {path}"),
            PendingOp::MoveNote { from, to } => format!("Move — {from} → {to}"),
            PendingOp::CreateFolder { path } => format!("New folder — {path}"),
            PendingOp::MoveFolder { from, to } => format!("Move folder — {from} → {to}"),
            PendingOp::DeleteFolder { path } => format!("Delete folder — {path}"),
        }
    }
}

/// A queued operation, with what the last attempt to send it said.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedOp {
    pub id: u64,
    pub queued_at: chrono::DateTime<chrono::Utc>,
    pub op: PendingOp,
    /// Why the last replay attempt failed, if it did. Kept so a change the
    /// server will never accept — an invalid path, say — is visible and
    /// discardable rather than retried forever in silence.
    #[serde(default)]
    pub last_error: Option<String>,
}

impl QueuedOp {
    pub fn new(id: u64, op: PendingOp) -> QueuedOp {
        QueuedOp {
            id,
            queued_at: now(),
            op,
            last_error: None,
        }
    }
}

/// `chrono::Utc::now()` works in the browser; this exists so the tests can be
/// written without one.
fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Compacts the queue without changing what replaying it does.
///
/// The rules, in the order they are applied to each incoming operation:
///
/// * A save merges into the previous save or create *for the same note*, as
///   long as nothing in between moved or deleted that note. The text is the
///   later one; the base hash stays the earlier one, because that is the
///   version the server actually has.
/// * A delete cancels the writes queued before it — their content is about to
///   be thrown away regardless.
/// * A note that was both created and deleted while offline never reached the
///   server at all, so both operations disappear.
///
/// Everything else keeps its place in the queue.
pub fn coalesce(ops: Vec<QueuedOp>) -> Vec<QueuedOp> {
    let mut out: Vec<QueuedOp> = Vec::with_capacity(ops.len());

    for queued in ops {
        match queued.op.clone() {
            PendingOp::SaveNote { path, markdown, .. } => {
                let start = segment_start(&out, &path);
                match last_touching(&out[start..], &path).map(|index| index + start) {
                    // Merge into whatever is already queued for this note,
                    // keeping that entry's base hash.
                    Some(index) => match &mut out[index].op {
                        PendingOp::SaveNote {
                            markdown: existing, ..
                        }
                        | PendingOp::CreateNote {
                            markdown: existing, ..
                        } => {
                            *existing = markdown;
                            out[index].queued_at = queued.queued_at;
                            out[index].last_error = None;
                        }
                        _ => out.push(queued),
                    },
                    None => out.push(queued),
                }
            }

            PendingOp::DeleteNote { path } => {
                let start = segment_start(&out, &path);
                let mut created_offline = false;
                let tail: Vec<QueuedOp> = out
                    .split_off(start)
                    .into_iter()
                    .filter(|queued| match &queued.op {
                        PendingOp::CreateNote { path: at, .. } if *at == path => {
                            created_offline = true;
                            false
                        }
                        PendingOp::SaveNote { path: at, .. } if *at == path => false,
                        _ => true,
                    })
                    .collect();
                out.extend(tail);

                if !created_offline {
                    out.push(queued);
                }
            }

            _ => out.push(queued),
        }
    }

    out
}

/// Index just past the last operation that changed what `path` refers to.
///
/// Compaction is confined to the segment after it: a save queued before a
/// rename is a save of a different file than one queued after it, even though
/// both name the same path.
fn segment_start(ops: &[QueuedOp], path: &str) -> usize {
    ops.iter()
        .rposition(|queued| queued.op.changes_identity() && queued.op.touches(path))
        .map(|index| index + 1)
        .unwrap_or(0)
}

fn last_touching(ops: &[QueuedOp], path: &str) -> Option<usize> {
    ops.iter().rposition(|queued| queued.op.touches(path))
}

/// Paths with work waiting on them, for marking tabs and tree rows.
pub fn pending_paths(ops: &[QueuedOp]) -> Vec<String> {
    let mut paths: Vec<String> = ops
        .iter()
        .map(|queued| queued.op.subject().to_string())
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    fn save(id: u64, path: &str, markdown: &str, base: &str) -> QueuedOp {
        QueuedOp::new(
            id,
            PendingOp::SaveNote {
                path: path.into(),
                markdown: markdown.into(),
                base_hash: base.into(),
            },
        )
    }

    fn create(id: u64, path: &str, markdown: &str) -> QueuedOp {
        QueuedOp::new(
            id,
            PendingOp::CreateNote {
                path: path.into(),
                markdown: markdown.into(),
            },
        )
    }

    fn delete(id: u64, path: &str) -> QueuedOp {
        QueuedOp::new(id, PendingOp::DeleteNote { path: path.into() })
    }

    fn move_note(id: u64, from: &str, to: &str) -> QueuedOp {
        QueuedOp::new(
            id,
            PendingOp::MoveNote {
                from: from.into(),
                to: to.into(),
            },
        )
    }

    /// The case that keeps the queue from growing without bound: autosave fires
    /// on every pause in typing, and all of those are one pending edit.
    #[test]
    fn repeated_saves_collapse_to_the_last_text() {
        let queue = coalesce(vec![
            save(1, "A.md", "one", "hash-0"),
            save(2, "A.md", "two", "hash-0"),
            save(3, "A.md", "three", "hash-0"),
        ]);

        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].op,
            PendingOp::SaveNote {
                path: "A.md".into(),
                markdown: "three".into(),
                base_hash: "hash-0".into(),
            }
        );
    }

    /// The base hash is the server's, so compaction must keep the *first* one.
    /// Taking the later value would send an If-Match the server never issued,
    /// and every replay would fail as a spurious conflict.
    #[test]
    fn compaction_keeps_the_earliest_base_hash() {
        let queue = coalesce(vec![
            save(1, "A.md", "one", "server-hash"),
            save(2, "A.md", "two", "some-other-hash"),
        ]);

        let PendingOp::SaveNote { base_hash, .. } = &queue[0].op else {
            panic!("expected a save");
        };
        assert_eq!(base_hash, "server-hash");
    }

    #[test]
    fn a_save_after_a_create_becomes_part_of_the_create() {
        let queue = coalesce(vec![
            create(1, "A.md", "# A\n"),
            save(2, "A.md", "# A\n\ntext", ""),
        ]);

        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0].op,
            PendingOp::CreateNote {
                path: "A.md".into(),
                markdown: "# A\n\ntext".into(),
            }
        );
    }

    /// A note written and then thrown away while offline is not something the
    /// server should ever hear about.
    #[test]
    fn a_note_created_and_deleted_offline_disappears() {
        let queue = coalesce(vec![
            create(1, "Scratch.md", "# Scratch\n"),
            save(2, "Scratch.md", "notes", ""),
            delete(3, "Scratch.md"),
        ]);

        assert!(queue.is_empty());
    }

    #[test]
    fn deleting_a_synced_note_drops_its_queued_edits_but_keeps_the_delete() {
        let queue = coalesce(vec![save(1, "A.md", "edited", "hash-0"), delete(2, "A.md")]);

        assert_eq!(queue.len(), 1);
        assert_eq!(queue[0].op, PendingOp::DeleteNote { path: "A.md".into() });
    }

    #[test]
    fn operations_on_different_notes_are_left_alone() {
        let queue = coalesce(vec![
            save(1, "A.md", "a", "h"),
            save(2, "B.md", "b", "h"),
            save(3, "A.md", "a2", "h"),
        ]);

        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].op.subject(), "A.md");
        assert_eq!(queue[1].op.subject(), "B.md");
    }

    /// Saves on either side of a rename are saves of the same file under two
    /// names, and the rename has to happen between them.
    #[test]
    fn a_rename_is_a_barrier_between_saves() {
        let queue = coalesce(vec![
            save(1, "A.md", "before", "hash-0"),
            move_note(2, "A.md", "B.md"),
            save(3, "B.md", "after", "hash-1"),
        ]);

        assert_eq!(queue.len(), 3);
    }

    /// Recreating a path after moving the original away is a different file, so
    /// the delete must not reach back past the move and cancel the first save.
    #[test]
    fn a_delete_does_not_cancel_work_from_before_a_move() {
        let queue = coalesce(vec![
            save(1, "A.md", "original", "hash-0"),
            move_note(2, "A.md", "B.md"),
            create(3, "A.md", "a fresh A"),
            delete(4, "A.md"),
        ]);

        // The fresh A and its delete cancel out; the original save and the move
        // both survive.
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].id, 1);
        assert_eq!(queue[1].id, 2);
    }

    /// A folder move carries its notes, so it is a barrier for them too.
    #[test]
    fn a_folder_move_is_a_barrier_for_notes_inside_it() {
        let queue = coalesce(vec![
            save(1, "Projects/A.md", "before", "hash-0"),
            QueuedOp::new(
                2,
                PendingOp::MoveFolder {
                    from: "Projects".into(),
                    to: "Archive/Projects".into(),
                },
            ),
            save(3, "Projects/A.md", "after", "hash-0"),
        ]);

        assert_eq!(queue.len(), 3);
    }

    #[test]
    fn pending_paths_are_unique_and_sorted() {
        let paths = pending_paths(&[
            save(1, "B.md", "x", "h"),
            save(2, "A.md", "y", "h"),
            delete(3, "A.md"),
        ]);
        assert_eq!(paths, vec!["A.md".to_string(), "B.md".to_string()]);
    }
}
