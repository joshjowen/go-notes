//! The syntax of a typed link, shared so the server and the frontend cannot
//! disagree about one.
//!
//! `[[contradicts::Kitchen Reno]]` says the same thing an ordinary
//! `[[Kitchen Reno]]` does and adds the author's word for *why*. The relation
//! is written before the target, following the convention Dataview established,
//! so that a note read as plain text over SSH still says what it means.
//!
//! This lives here rather than in the markdown parser because the frontend has
//! to reach the same verdict when it is offline and scanning its own cached
//! notes. `crates/shared/src/paths.rs` exists for the same reason, and the
//! reasoning is the same: two implementations of one rule drift, and the
//! symptom is a link that resolves on the server and not on the device.
//!
//! There is a third implementation, in `editor/src/wikilink-mdast.ts`, which
//! cannot share this code because it is TypeScript. It carries a comment
//! pointing here, and `editor/test/roundtrip.test.mjs` covers the same cases
//! `relation_tests` below does.

/// Longest relation this will accept.
///
/// Not a storage limit — it is what keeps the rule below narrow. A label is a
/// word or three; anything longer is far likelier to be a target that happens
/// to contain a colon.
const MAX_RELATION: usize = 32;

/// Splits `relation::target` into its two halves.
///
/// Returns `(None, whole)` unless the text before `::` really looks like a
/// relation label. That guard is the whole point of the function: `::` is legal
/// in a filename, so a vault that already contains `[[C++::Notes]]` must keep
/// meaning what it meant before typed links existed. When in doubt this decides
/// the link is untyped, because inventing a relation changes what the graph
/// claims the author said, while missing one only fails to add a label.
///
/// The target is returned untrimmed; callers trim it as they already did.
pub fn split_relation(inner: &str) -> (Option<&str>, &str) {
    let Some((before, after)) = inner.split_once("::") else {
        return (None, inner);
    };

    let relation = before.trim();
    if !is_relation_label(relation) || after.trim().is_empty() {
        return (None, inner);
    }

    (Some(relation), after)
}

/// Whether a label is one this will treat as a relation.
///
/// Deliberately strict: it must start with a letter, and may then contain
/// letters, digits, spaces, hyphens and underscores. No dots, no slashes, no
/// colons — everything a path is made of is excluded, so a path can never be
/// mistaken for a relation.
///
/// It cannot be made total, and it is worth being clear about where it stops.
/// `[[std::vector]]` is read as the relation `std` pointing at `vector`,
/// because a bare identifier is exactly the shape a relation has; nothing about
/// the text says which was meant. Two things keep that from mattering much:
/// pass one already excludes code spans and fenced blocks, which is where
/// namespaced identifiers nearly always appear, and an author who does want a
/// note called `std::vector` can write `[[./std::vector]]` — the leading `./`
/// is not a label character, so the split is refused, and `normalize_target_key`
/// strips it back off again.
pub fn is_relation_label(label: &str) -> bool {
    if label.is_empty() || label.chars().count() > MAX_RELATION {
        return false;
    }
    let mut chars = label.chars();
    let first = chars.next().expect("just checked it is non-empty");
    if !first.is_alphabetic() {
        return false;
    }
    label.chars().all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
}

#[cfg(test)]
mod relation_tests {
    use super::*;

    #[test]
    fn a_relation_is_split_off_the_front() {
        assert_eq!(split_relation("contradicts::Kitchen"), (Some("contradicts"), "Kitchen"));
        assert_eq!(split_relation("relates to::Budget"), (Some("relates to"), "Budget"));
        assert_eq!(split_relation("follows-up::A/B"), (Some("follows-up"), "A/B"));
    }

    #[test]
    fn surrounding_space_is_allowed_around_the_relation() {
        assert_eq!(split_relation(" supersedes :: Old Plan"), (Some("supersedes"), " Old Plan"));
    }

    #[test]
    fn an_untyped_link_is_returned_whole() {
        assert_eq!(split_relation("Kitchen Reno"), (None, "Kitchen Reno"));
        assert_eq!(split_relation("Projects/Kitchen"), (None, "Projects/Kitchen"));
    }

    // The reason the guard exists: these all contain `::` and none of them is a
    // typed link. Reading them as one would silently rewrite what the note says.
    #[test]
    fn a_target_that_merely_contains_a_colon_pair_is_not_typed() {
        assert_eq!(split_relation("C++::Notes"), (None, "C++::Notes"));
        assert_eq!(split_relation("9to5::Work"), (None, "9to5::Work"));
        assert_eq!(split_relation("a.b::c"), (None, "a.b::c"));
        assert_eq!(split_relation("Notes/Deep::Dive"), (None, "Notes/Deep::Dive"));
    }

    // Where the guard stops, stated as a test so it is a decision rather than a
    // surprise: a bare identifier is the same shape as a relation, so this one
    // is read as typed and the escape hatch is the documented `./` prefix.
    #[test]
    fn a_bare_identifier_before_the_colons_is_read_as_a_relation() {
        assert_eq!(split_relation("std::vector"), (Some("std"), "vector"));
        assert_eq!(split_relation("./std::vector"), (None, "./std::vector"));
    }

    #[test]
    fn a_relation_with_no_target_is_not_a_link_type() {
        // `[[relates::]]` names nothing, so it stays whatever text it was.
        assert_eq!(split_relation("relates::"), (None, "relates::"));
        assert_eq!(split_relation("relates::   "), (None, "relates::   "));
    }

    #[test]
    fn an_overlong_label_is_a_target_not_a_relation() {
        let long = "a".repeat(MAX_RELATION + 1);
        let input = format!("{long}::Note");
        assert_eq!(split_relation(&input), (None, input.as_str()));

        let ok = "a".repeat(MAX_RELATION);
        let input = format!("{ok}::Note");
        assert_eq!(split_relation(&input), (Some(ok.as_str()), "Note"));
    }

    #[test]
    fn only_the_first_pair_of_colons_splits() {
        // The target keeps its own colons, which is what makes the C++ case above
        // survive once a genuine relation is put in front of it.
        assert_eq!(split_relation("cites::std::vector"), (Some("cites"), "std::vector"));
    }

    #[test]
    fn labels_are_recognised_by_shape_not_by_a_list() {
        assert!(is_relation_label("contradicts"));
        assert!(is_relation_label("relates to"));
        assert!(is_relation_label("part_of"));
        assert!(!is_relation_label(""));
        assert!(!is_relation_label("2nd"));
        assert!(!is_relation_label("has/slash"));
        assert!(!is_relation_label("has.dot"));
    }
}
