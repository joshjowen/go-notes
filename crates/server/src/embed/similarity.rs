//! Comparing passages, and reducing the comparisons to edges between notes.
//!
//! Everything here is pure arithmetic over slices, and that is the point: the
//! rest of this module needs a database and a model to say anything at all,
//! while this is where the decisions live that are worth being sure about. It is
//! the only part of the embedding feature `cargo test` can reach.
//!
//! Vectors arrive L2-normalised, so cosine similarity is a dot product and the
//! result is already in `-1..=1`. Nothing here re-normalises: doing it twice
//! would hide a bug in whoever failed to do it once.

use uuid::Uuid;

/// One passage, ready to compare.
pub struct Passage {
    pub note_id: Uuid,
    pub ordinal: i32,
    pub vector: Vec<f32>,
}

/// An edge the graph can draw, after the reduction below.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbour {
    pub target_note_id: Uuid,
    pub score: f32,
    pub source_ordinal: i32,
    pub target_ordinal: i32,
}

/// Cosine similarity of two normalised vectors.
///
/// Mismatched lengths return 0 rather than panicking or comparing a prefix: it
/// means two different models' output met, and the honest answer to "how similar
/// are these" is "no idea", not a number derived from half a vector.
pub fn similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Scales a vector to unit length, in place.
///
/// A zero vector is left alone. It should never occur — a model that returns one
/// has failed — but dividing by its length would put NaN into the database,
/// where it would spread to every comparison it touched.
pub fn normalise(vector: &mut [f32]) {
    let magnitude = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if magnitude > f32::EPSILON {
        for value in vector.iter_mut() {
            *value /= magnitude;
        }
    }
}

/// The strongest connections from one note to any other.
///
/// Reduces passage-to-passage comparisons to note-to-note edges by keeping, for
/// each other note, only its best-matching pair of passages. Two notes that
/// overlap in six places are still one relationship, and six parallel edges
/// would say nothing the strongest one does not.
///
/// Passages belonging to `source_note` are skipped: every note resembles itself,
/// and a self-loop is not something the layout can draw.
pub fn best_neighbours(
    source: &[Passage],
    corpus: &[Passage],
    source_note: Uuid,
    limit: usize,
    min_score: f32,
) -> Vec<Neighbour> {
    let mut best: std::collections::HashMap<Uuid, Neighbour> = std::collections::HashMap::new();

    for mine in source {
        for theirs in corpus {
            if theirs.note_id == source_note {
                continue;
            }
            let score = similarity(&mine.vector, &theirs.vector);
            if score < min_score {
                continue;
            }
            let entry = best.entry(theirs.note_id).or_insert(Neighbour {
                target_note_id: theirs.note_id,
                score: f32::MIN,
                source_ordinal: mine.ordinal,
                target_ordinal: theirs.ordinal,
            });
            if score > entry.score {
                entry.score = score;
                entry.source_ordinal = mine.ordinal;
                entry.target_ordinal = theirs.ordinal;
            }
        }
    }

    let mut found: Vec<Neighbour> = best.into_values().collect();
    // Score first, then the note id, so a vault with ties produces the same
    // graph on every rebuild rather than one that reshuffles for no reason.
    found.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.target_note_id.cmp(&b.target_note_id))
    });
    found.truncate(limit);
    found
}

/// Packs a normalised vector for storage: little-endian f32, no header.
pub fn pack(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

/// Unpacks what `pack` wrote.
///
/// A length that is not a multiple of four means the column holds something this
/// did not write, so it yields nothing rather than a vector of plausible
/// nonsense that would quietly produce wrong edges.
pub fn unpack(bytes: &[u8]) -> Vec<f32> {
    if bytes.len() % 4 != 0 {
        return Vec::new();
    }
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passage(note: Uuid, ordinal: i32, vector: &[f32]) -> Passage {
        let mut vector = vector.to_vec();
        normalise(&mut vector);
        Passage {
            note_id: note,
            ordinal,
            vector,
        }
    }

    #[test]
    fn identical_vectors_score_one_and_opposite_ones_score_minus_one() {
        let mut a = vec![1.0, 2.0, 3.0];
        let mut b = vec![1.0, 2.0, 3.0];
        let mut c = vec![-1.0, -2.0, -3.0];
        normalise(&mut a);
        normalise(&mut b);
        normalise(&mut c);

        assert!((similarity(&a, &b) - 1.0).abs() < 1e-6);
        assert!((similarity(&a, &c) + 1.0).abs() < 1e-6);
    }

    #[test]
    fn orthogonal_vectors_score_zero() {
        let mut a = vec![1.0, 0.0];
        let mut b = vec![0.0, 1.0];
        normalise(&mut a);
        normalise(&mut b);
        assert!(similarity(&a, &b).abs() < 1e-6);
    }

    /// Two different models met. Comparing a prefix would give a number that
    /// looks like an answer, which is worse than admitting there isn't one.
    #[test]
    fn vectors_of_different_lengths_do_not_compare() {
        assert_eq!(similarity(&[1.0, 0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(similarity(&[], &[]), 0.0);
    }

    /// A model returning zeros is broken, but NaN in the database would spread
    /// to every comparison it touched, so it must not be possible from here.
    #[test]
    fn a_zero_vector_does_not_become_nan() {
        let mut zero = vec![0.0, 0.0, 0.0];
        normalise(&mut zero);
        assert!(zero.iter().all(|v| *v == 0.0));
        assert!(!similarity(&zero, &[1.0, 0.0, 0.0]).is_nan());
    }

    #[test]
    fn a_note_never_links_to_itself() {
        let me = Uuid::from_u128(1);
        let mine = vec![passage(me, 0, &[1.0, 0.0])];
        let corpus = vec![passage(me, 0, &[1.0, 0.0]), passage(me, 1, &[1.0, 0.0])];

        assert!(best_neighbours(&mine, &corpus, me, 5, 0.5).is_empty());
    }

    /// Two notes overlapping in several places are one relationship. Keeping the
    /// best pair is what makes the score mean "how close are these at their
    /// closest" rather than an average that hides the match entirely.
    #[test]
    fn several_matching_passages_collapse_to_the_strongest_one() {
        let me = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);

        let mine = vec![passage(me, 0, &[1.0, 0.0]), passage(me, 1, &[0.0, 1.0])];
        let corpus = vec![
            passage(other, 7, &[0.9, 0.1]),
            passage(other, 8, &[0.0, 1.0]), // an exact match for ordinal 1
        ];

        let found = best_neighbours(&mine, &corpus, me, 5, 0.0);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].target_note_id, other);
        assert_eq!(found[0].source_ordinal, 1);
        assert_eq!(found[0].target_ordinal, 8);
        assert!((found[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn weak_matches_are_cut_by_the_threshold() {
        let me = Uuid::from_u128(1);
        let other = Uuid::from_u128(2);
        let mine = vec![passage(me, 0, &[1.0, 0.0])];
        let corpus = vec![passage(other, 0, &[0.2, 1.0])];

        assert!(!best_neighbours(&mine, &corpus, me, 5, 0.1).is_empty());
        assert!(best_neighbours(&mine, &corpus, me, 5, 0.9).is_empty());
    }

    #[test]
    fn only_the_strongest_neighbours_are_kept() {
        let me = Uuid::from_u128(1);
        let mine = vec![passage(me, 0, &[1.0, 0.0])];
        let corpus: Vec<Passage> = (2..8)
            .map(|i| passage(Uuid::from_u128(i), 0, &[1.0, (i as f32 - 1.0) / 10.0]))
            .collect();

        let found = best_neighbours(&mine, &corpus, me, 3, 0.0);
        assert_eq!(found.len(), 3);
        // Sorted strongest first, and the strongest is the least skewed one.
        assert_eq!(found[0].target_note_id, Uuid::from_u128(2));
        assert!(found[0].score >= found[1].score);
        assert!(found[1].score >= found[2].score);
    }

    /// A rebuild must produce the same graph, so ties break on something stable
    /// rather than on hash iteration order.
    #[test]
    fn ties_break_deterministically() {
        let me = Uuid::from_u128(1);
        let mine = vec![passage(me, 0, &[1.0, 0.0])];
        let corpus: Vec<Passage> = (2..6)
            .map(|i| passage(Uuid::from_u128(i), 0, &[1.0, 0.0]))
            .collect();

        let first = best_neighbours(&mine, &corpus, me, 2, 0.0);
        for _ in 0..20 {
            assert_eq!(best_neighbours(&mine, &corpus, me, 2, 0.0), first);
        }
        assert_eq!(first[0].target_note_id, Uuid::from_u128(2));
        assert_eq!(first[1].target_note_id, Uuid::from_u128(3));
    }

    #[test]
    fn packing_round_trips() {
        let vector = vec![0.5, -0.25, 0.125];
        assert_eq!(unpack(&pack(&vector)), vector);
        assert!(unpack(&[1, 2, 3]).is_empty());
        assert!(unpack(&[]).is_empty());
    }

    #[test]
    fn nothing_to_compare_against_yields_nothing() {
        let me = Uuid::from_u128(1);
        assert!(best_neighbours(&[], &[], me, 5, 0.0).is_empty());
        assert!(best_neighbours(&[passage(me, 0, &[1.0])], &[], me, 5, 0.0).is_empty());
    }
}
