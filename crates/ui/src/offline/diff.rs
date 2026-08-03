//! A line diff, used to show what actually differs when two versions of a note
//! collide.
//!
//! Asking someone to choose between "your version" and "the version on the
//! server" without showing them what changed is asking them to guess. This is
//! the smallest thing that makes the choice informed: a line-level diff, the
//! same shape `diff` itself produces.
//!
//! The algorithm is a plain longest-common-subsequence, which is quadratic. Two
//! things keep that honest: identical prefixes and suffixes are stripped first
//! (which is nearly all of a document when someone has edited one paragraph),
//! and beyond [`MAX_DIFF_LINES`] of *remaining* difference the comparison stops
//! being useful to read anyway, so it degrades to "this whole region was
//! replaced" rather than spending seconds on a matrix nobody will study.

/// Which side a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// Present in both versions.
    Same,
    /// Only in the local version.
    Mine,
    /// Only in the server's version.
    Theirs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// Above this many differing lines on either side, fall back to showing the
/// changed region as a wholesale replacement.
pub const MAX_DIFF_LINES: usize = 1200;

/// One element of a line-by-line alignment between two texts: present in
/// both, or only on the left (`mine`, in the two-way diff's terms) or only on
/// the right (`theirs`).
///
/// Generic over what "left" and "right" mean so the same alignment serves the
/// two-way diff shown in the conflict dialog (`diff_lines`, below) and the
/// three-way merge in [`super::merge`] that tries to avoid showing one —
/// there `left` is "base vs. mine" and, separately, "base vs. theirs".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aligned<'a> {
    Same(&'a str),
    Left(&'a str),
    Right(&'a str),
}

/// Aligns `left` and `right` line by line.
///
/// Identical head and tail are stripped first (most of a document, after a
/// small edit), and beyond [`MAX_DIFF_LINES`] of remaining difference the
/// comparison gives up on being useful to read and reports the whole middle
/// as replaced rather than spending a quadratic amount of work on it.
pub fn align<'a>(left: &'a str, right: &'a str) -> Vec<Aligned<'a>> {
    let left: Vec<&str> = left.lines().collect();
    let right: Vec<&str> = right.lines().collect();

    let head = left
        .iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let tail = left[head..]
        .iter()
        .rev()
        .zip(right[head..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();

    let left_middle = &left[head..left.len() - tail];
    let right_middle = &right[head..right.len() - tail];

    let mut out: Vec<Aligned> = Vec::new();
    out.extend(left[..head].iter().map(|line| Aligned::Same(line)));

    if left_middle.len() > MAX_DIFF_LINES || right_middle.len() > MAX_DIFF_LINES {
        // Right before left, matching the reading order of the conflict
        // dialog this feeds: "theirs", then "mine".
        out.extend(right_middle.iter().map(|line| Aligned::Right(line)));
        out.extend(left_middle.iter().map(|line| Aligned::Left(line)));
    } else {
        out.extend(lcs_align(left_middle, right_middle));
    }

    out.extend(left[left.len() - tail..].iter().map(|line| Aligned::Same(line)));
    out
}

/// Compares two versions of a note, line by line.
///
/// `mine` is the local version, `theirs` the server's; the result is in
/// document order with unchanged lines included, so it can be rendered as a
/// unified diff.
pub fn diff_lines(mine: &str, theirs: &str) -> Vec<DiffLine> {
    align(mine, theirs)
        .into_iter()
        .map(|piece| match piece {
            Aligned::Same(text) => same(text),
            Aligned::Left(text) => line_of(DiffKind::Mine, text),
            Aligned::Right(text) => line_of(DiffKind::Theirs, text),
        })
        .collect()
}

/// How many lines differ, for a one-line summary above the diff.
pub fn change_counts(diff: &[DiffLine]) -> (usize, usize) {
    let mine = diff.iter().filter(|line| line.kind == DiffKind::Mine).count();
    let theirs = diff
        .iter()
        .filter(|line| line.kind == DiffKind::Theirs)
        .count();
    (mine, theirs)
}

fn same(text: &str) -> DiffLine {
    line_of(DiffKind::Same, text)
}

fn line_of(kind: DiffKind, text: &str) -> DiffLine {
    DiffLine {
        kind,
        text: text.to_string(),
    }
}

/// Classic LCS table, walked backwards to produce the edit script.
fn lcs_align<'a>(left: &[&'a str], right: &[&'a str]) -> Vec<Aligned<'a>> {
    let rows = left.len();
    let columns = right.len();

    // table[i][j] = length of the longest common subsequence of left[i..] and
    // right[j..]. Built from the end so the walk below runs forwards, which
    // keeps the output in document order without a reversal.
    let mut table = vec![vec![0usize; columns + 1]; rows + 1];
    for i in (0..rows).rev() {
        for j in (0..columns).rev() {
            table[i][j] = if left[i] == right[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < rows && j < columns {
        if left[i] == right[j] {
            out.push(Aligned::Same(left[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            out.push(Aligned::Left(left[i]));
            i += 1;
        } else {
            out.push(Aligned::Right(right[j]));
            j += 1;
        }
    }
    out.extend(left[i..].iter().map(|line| Aligned::Left(line)));
    out.extend(right[j..].iter().map(|line| Aligned::Right(line)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(diff: &[DiffLine]) -> Vec<String> {
        diff.iter()
            .map(|line| {
                let marker = match line.kind {
                    DiffKind::Same => ' ',
                    DiffKind::Mine => '<',
                    DiffKind::Theirs => '>',
                };
                format!("{marker}{}", line.text)
            })
            .collect()
    }

    #[test]
    fn identical_documents_are_all_context() {
        let diff = diff_lines("a\nb\nc\n", "a\nb\nc\n");
        assert!(diff.iter().all(|line| line.kind == DiffKind::Same));
        assert_eq!(diff.len(), 3);
    }

    #[test]
    fn a_changed_line_shows_both_versions_in_place() {
        let diff = diff_lines("a\nmine\nc\n", "a\ntheirs\nc\n");
        assert_eq!(
            render(&diff),
            vec![" a", "<mine", ">theirs", " c"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_added_paragraph_is_one_sided() {
        let diff = diff_lines("a\nb\nextra\n", "a\nb\n");
        let (mine, theirs) = change_counts(&diff);
        assert_eq!((mine, theirs), (1, 0));
        assert_eq!(diff.last().unwrap().text, "extra");
    }

    #[test]
    fn a_deleted_paragraph_is_one_sided() {
        let diff = diff_lines("a\n", "a\ngone\n");
        let (mine, theirs) = change_counts(&diff);
        assert_eq!((mine, theirs), (0, 1));
    }

    #[test]
    fn an_empty_local_version_is_all_theirs() {
        let diff = diff_lines("", "a\nb\n");
        assert_eq!(change_counts(&diff), (0, 2));
    }

    /// Interleaved edits still line up against the shared lines rather than
    /// being reported as "everything changed".
    #[test]
    fn shared_lines_survive_edits_around_them() {
        let diff = diff_lines("one\nMINE\ntwo\nthree\n", "one\ntwo\nTHEIRS\nthree\n");
        let context: Vec<&str> = diff
            .iter()
            .filter(|line| line.kind == DiffKind::Same)
            .map(|line| line.text.as_str())
            .collect();
        assert_eq!(context, vec!["one", "two", "three"]);
    }

    /// Two very large, entirely different documents must not spend a quadratic
    /// amount of work; the fallback reports them as a wholesale replacement.
    #[test]
    fn very_large_differences_fall_back_to_a_wholesale_replacement() {
        let mine: String = (0..MAX_DIFF_LINES + 10)
            .map(|n| format!("mine {n}\n"))
            .collect();
        let theirs: String = (0..MAX_DIFF_LINES + 10)
            .map(|n| format!("theirs {n}\n"))
            .collect();

        let diff = diff_lines(&mine, &theirs);
        assert!(diff.iter().all(|line| line.kind != DiffKind::Same));
        let (mine_lines, theirs_lines) = change_counts(&diff);
        assert_eq!(mine_lines, MAX_DIFF_LINES + 10);
        assert_eq!(theirs_lines, MAX_DIFF_LINES + 10);
    }

    /// A one-line change in a huge note is cheap, because the identical head and
    /// tail never reach the matrix.
    #[test]
    fn a_small_edit_in_a_large_note_is_still_a_real_diff() {
        let mut lines: Vec<String> = (0..5_000).map(|n| format!("line {n}")).collect();
        let theirs = lines.join("\n");
        lines[2_500] = "changed".into();
        let mine = lines.join("\n");

        let diff = diff_lines(&mine, &theirs);
        assert_eq!(change_counts(&diff), (1, 1));
    }
}
