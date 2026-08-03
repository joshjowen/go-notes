//! A conservative three-way merge, tried before a save conflict is shown.
//!
//! Typing a link, then backspacing into it, sends autosaves fast enough that
//! two overlapping saves are common — see `crate::save`. Most of the 409s that
//! produces are not really conflicts: two changes to different paragraphs of
//! the same note, one from this tab and one from another device, that could
//! simply both be kept. This is the check that tells those apart from an
//! actual disagreement.
//!
//! It only ever runs when the caller holds the *text* of the common ancestor,
//! not just its hash — the live-editor save path does, the offline replay
//! queue does not (see `crate::offline::sync`, which keeps stopping at the
//! first conflict and asking a person, exactly as before). Nothing here picks
//! a winner between two edits to the same line; that is still a decision for
//! whoever is typing.

use super::diff::{align, Aligned};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Merge {
    /// No edit on either side landed in the same region as an edit on the
    /// other; here is the result of applying both.
    Clean(String),
    /// The same region of the note was changed differently on both sides.
    /// Left for a person.
    Conflicted,
}

/// A contiguous change against `base`: the lines `base[base_start..base_end]`
/// were replaced by `replacement` (which may be shorter, longer, or empty —
/// a delete, an insert, or an ordinary edit are all just this).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    base_start: usize,
    base_end: usize,
    replacement: Vec<String>,
}

/// Merges `mine` and `theirs`, both descended from `base`.
pub fn three_way(base: &str, mine: &str, theirs: &str) -> Merge {
    // The common cases first, and cheaply: nothing to merge, or only one side
    // touched the note at all.
    if mine == theirs {
        return Merge::Clean(mine.to_string());
    }
    if mine == base {
        return Merge::Clean(theirs.to_string());
    }
    if theirs == base {
        return Merge::Clean(mine.to_string());
    }

    let base_lines: Vec<&str> = base.lines().collect();
    let mine_hunks = hunks_of(&align(base, mine));
    let theirs_hunks = hunks_of(&align(base, theirs));

    for mine_hunk in &mine_hunks {
        for theirs_hunk in &theirs_hunks {
            if !touches_or_overlaps(mine_hunk, theirs_hunk) {
                continue;
            }
            // The same edit, made independently on both sides, is not a
            // disagreement — count it once rather than asking about it.
            if mine_hunk != theirs_hunk {
                return Merge::Conflicted;
            }
        }
    }

    Merge::Clean(apply(&base_lines, &mine_hunks, &theirs_hunks, base.ends_with('\n')))
}

/// Groups a base/other alignment into hunks: a run of `Left`/`Right` pieces
/// with no `Same` line inside it is one edit, however many deletions and
/// insertions the run mixes together.
fn hunks_of(alignment: &[Aligned]) -> Vec<Hunk> {
    let mut hunks = Vec::new();
    let mut current: Option<Hunk> = None;
    let mut base_index = 0usize;

    for piece in alignment {
        match piece {
            Aligned::Same(_) => {
                if let Some(hunk) = current.take() {
                    hunks.push(hunk);
                }
                base_index += 1;
            }
            // Present in base, not in the other version: a deletion.
            Aligned::Left(_) => {
                let hunk = current.get_or_insert_with(|| Hunk {
                    base_start: base_index,
                    base_end: base_index,
                    replacement: Vec::new(),
                });
                hunk.base_end = base_index + 1;
                base_index += 1;
            }
            // Present in the other version, not in base: an insertion.
            Aligned::Right(text) => {
                let hunk = current.get_or_insert_with(|| Hunk {
                    base_start: base_index,
                    base_end: base_index,
                    replacement: Vec::new(),
                });
                hunk.replacement.push((*text).to_string());
            }
        }
    }
    if let Some(hunk) = current.take() {
        hunks.push(hunk);
    }
    hunks
}

/// Whether two hunks share a base line, or sit back to back with no unchanged
/// line between them.
///
/// Touching counts as overlapping deliberately: two edits that land on
/// exactly adjacent lines are exactly the case an automatic line-based merge
/// cannot tell apart from one edit that got split by coincidence, so it is
/// not guessed at — it is treated the same as a real overlap.
fn touches_or_overlaps(a: &Hunk, b: &Hunk) -> bool {
    a.base_start <= b.base_end && b.base_start <= a.base_end
}

/// Applies every hunk from both sides onto `base`, in document order,
/// counting an edit both sides made identically only once.
fn apply(base: &[&str], mine_hunks: &[Hunk], theirs_hunks: &[Hunk], trailing_newline: bool) -> String {
    let mut hunks: Vec<&Hunk> = Vec::new();
    for hunk in mine_hunks.iter().chain(theirs_hunks.iter()) {
        if !hunks.iter().any(|existing| *existing == hunk) {
            hunks.push(hunk);
        }
    }
    hunks.sort_by_key(|hunk| hunk.base_start);

    let mut out: Vec<String> = Vec::new();
    let mut cursor = 0usize;
    for hunk in hunks {
        out.extend(base[cursor..hunk.base_start].iter().map(|line| line.to_string()));
        out.extend(hunk.replacement.iter().cloned());
        cursor = hunk.base_end;
    }
    out.extend(base[cursor..].iter().map(|line| line.to_string()));

    let mut text = out.join("\n");
    if trailing_newline && !text.is_empty() {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_on_each_side_in_different_paragraphs_merges_without_asking() {
        let base = "# Notes\n\nFirst paragraph.\n\nSecond paragraph.\n";
        let mine = "# Notes\n\nFirst paragraph, expanded.\n\nSecond paragraph.\n";
        let theirs = "# Notes\n\nFirst paragraph.\n\nSecond paragraph, expanded.\n";

        let merged = three_way(base, mine, theirs);
        assert_eq!(
            merged,
            Merge::Clean(
                "# Notes\n\nFirst paragraph, expanded.\n\nSecond paragraph, expanded.\n"
                    .to_string()
            )
        );
    }

    #[test]
    fn the_same_line_changed_two_ways_is_left_for_a_person() {
        let base = "one\ntwo\nthree\n";
        let mine = "one\nMINE\nthree\n";
        let theirs = "one\nTHEIRS\nthree\n";

        assert_eq!(three_way(base, mine, theirs), Merge::Conflicted);
    }

    #[test]
    fn adjacent_changes_are_treated_as_overlapping_rather_than_guessed_at() {
        let base = "a\nb\nc\nd\n";
        // Mine changes `b`, theirs changes the very next line `c` — no
        // unchanged base line separates the two edits.
        let mine = "a\nMINE\nc\nd\n";
        let theirs = "a\nb\nTHEIRS\nd\n";

        assert_eq!(three_way(base, mine, theirs), Merge::Conflicted);
    }

    #[test]
    fn an_edit_only_one_side_made_survives_the_merge() {
        let base = "a\nb\nc\n";
        let mine = "a\nb\nc\n";
        let theirs = "a\nCHANGED\nc\n";

        assert_eq!(three_way(base, mine, theirs), Merge::Clean(theirs.to_string()));
    }

    #[test]
    fn merging_a_document_with_itself_changes_nothing() {
        let text = "# Same\n\nNo edits anywhere.\n";
        assert_eq!(three_way(text, text, text), Merge::Clean(text.to_string()));
    }

    #[test]
    fn insertions_on_both_sides_in_different_places_both_land() {
        let base = "a\nb\nc\n";
        let mine = "a\nb\nb2\nc\n";
        let theirs = "a\na2\nb\nc\n";

        assert_eq!(
            three_way(base, mine, theirs),
            Merge::Clean("a\na2\nb\nb2\nc\n".to_string())
        );
    }
}
