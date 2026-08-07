//! Splitting a note into the passages an embedding model sees.
//!
//! A whole note is the wrong unit. Averaging a two-thousand-word page into one
//! vector produces something that is a little bit like everything and close to
//! nothing, and it cannot answer the question the graph is actually being asked:
//! *which part* of this note relates to that one. A single paragraph is the
//! wrong unit too — "It depends." is a sentence with no meaning outside the
//! heading it sits under.
//!
//! So the unit here is a run of prose under a heading, capped at a length, with
//! the heading path prefixed to the text that gets embedded. That prefix is not
//! decoration: it is what lets a paragraph carry the topic it was written under
//! into a comparison with a paragraph from somewhere else entirely.
//!
//! Two things are deliberately excluded: code blocks (a fenced shell transcript
//! is similar to every other fenced shell transcript and would link unrelated
//! notes through their tooling) and frontmatter (tags and titles are already
//! indexed as themselves). A fenced block also always ends the passage around
//! it, whether or not its content gets embedded — prose on either side of a
//! diagram or a shell transcript is not one continuous thought just because the
//! thing between them doesn't get embedded.
//!
//! Mermaid fences are the one exception to full exclusion: their structural
//! syntax — arrows, diagram-type keywords — is exactly the boilerplate the rule
//! above exists to avoid, but the label text between it is real, diagram-
//! specific content (a C4 diagram of one system's architecture reads nothing
//! like another's), so it is extracted and embedded as its own passage rather
//! than dropped with everything else that's fenced. See `is_mermaid_language`
//! and `extract_mermaid_labels`.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Parser, Tag, TagEnd};

/// A passage of a note, ready to be embedded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position in the note, from zero. Stable for unchanged text.
    pub ordinal: i32,
    /// The heading path this sat under, `"Projects > Kitchen"`, or empty.
    pub heading: String,
    /// The prose itself, without the heading.
    pub body: String,
}

impl Chunk {
    /// What actually goes to the model: the heading path, then the prose.
    ///
    /// Kept as a method rather than stored, so the stored text stays the note's
    /// own words and changing this formatting does not rewrite every row.
    pub fn embedding_text(&self) -> String {
        if self.heading.is_empty() {
            self.body.clone()
        } else {
            format!("{}\n\n{}", self.heading, self.body)
        }
    }
}

/// Longest passage sent to a model, in characters.
///
/// A constant rather than a setting, because chunking happens inside the
/// indexing transaction — where there is no configuration in hand, and threading
/// it through every caller of `index_note_content` would be a large change to
/// make one number adjustable. Roughly 375 tokens, which is comfortably inside
/// the window of every embedding model worth pointing this at, so there is no
/// model that needs it lowered and no obvious gain from raising it: longer
/// passages blur together rather than matching more precisely.
pub const DEFAULT_CHUNK_CHARS: usize = 1500;

/// Shortest passage worth embedding, in characters.
///
/// "Yes.", "TODO", and a bare link are all things a vector cannot say anything
/// useful about, and each one would sit near every other short fragment in the
/// vault, producing edges between notes that have nothing in common but brevity.
const MIN_CHARS: usize = 80;

/// `mermaid`, however it was written in the fence.
///
/// Mirrors `editor/src/mermaid.ts`'s `isMermaidLanguage` exactly — trim, fold
/// case, exact match, no substring match — so a fence the editor renders as a
/// diagram is the same fence this embeds as one. Kept in sync by hand: there is
/// no way for Rust to call the TypeScript or vice versa, so a change to one must
/// be paired with the other, the same as `editor/src/wikilink-mdast.ts` and
/// `crates/shared/src/links.rs` are for link syntax.
fn is_mermaid_language(language: &str) -> bool {
    language.trim().eq_ignore_ascii_case("mermaid")
}

/// Lines that name the diagram rather than say anything about it. Matched
/// whole, case-folded, once any trailing direction word has been split off.
const MERMAID_DECLARATIONS: &[&str] = &[
    "graph",
    "flowchart",
    "sequencediagram",
    "classdiagram",
    "statediagram",
    "statediagram-v2",
    "gantt",
    "gitgraph",
    "erdiagram",
    "journey",
    "pie",
    "mindmap",
    "timeline",
    "c4context",
    "c4container",
    "c4component",
    "c4dynamic",
    "c4deployment",
];

/// Direction tokens (`graph TD`, `flowchart LR`) that carry no content on
/// their own, whether they trail a declaration or, more rarely, sit alone.
const MERMAID_DIRECTIONS: &[&str] = &["td", "tb", "lr", "rl", "bt"];

/// Connector tokens stripped from every surviving line. Ordered so that a
/// longer token is always replaced before any shorter token that is a prefix
/// or substring of it (`-->>` before `-->`, `--x` before `--`) — replacing in
/// the wrong order would leave a mangled fragment of the longer arrow behind.
const MERMAID_ARROWS: &[&str] = &[
    "-.->", "==>", "-->>", "->>", "--x", "--)", "-.-", "..>", "-->", "---", "->", "--",
];

/// Pulls the human-authored label text out of a mermaid diagram, discarding
/// the syntax around it.
///
/// Uniform across every mermaid dialect on purpose — no per-dialect keyword
/// list. A flowchart's node labels and a sequence diagram's participant names
/// are both real content; the arrows and declarations around them are not, and
/// stripping only those is enough to stop two unrelated diagrams from scoring
/// similar purely because they are both diagrams. Getting every dialect's own
/// furniture out (`participant`, `activate`, a class method's return type) is
/// deliberately out of scope: that is a per-dialect grammar, not a shared
/// syntax, and chasing it would mean a small parser per diagram type for a
/// feature that only needs to be roughly right. A bare node ID with no label
/// (`A`, `B1`) leaks through as noise; short and generic, it does not dominate
/// a comparison the way a full boilerplate transcript would.
fn extract_mermaid_labels(source: &str) -> String {
    let mut lines = Vec::new();

    for raw_line in source.lines() {
        let mut line = raw_line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        // A leading declaration names the diagram, not its content — strip it
        // and whatever direction token follows on the same line.
        if let Some(first) = line.split_whitespace().next() {
            if MERMAID_DECLARATIONS.contains(&first.to_ascii_lowercase().as_str()) {
                line = line
                    .split_whitespace()
                    .skip(1)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
        }

        // A direction token, alone on what's left of the line (whether it
        // trailed a declaration just stripped above, or rarely sits by
        // itself), carries nothing worth keeping.
        let mut words = line.split_whitespace();
        if let (Some(only), None) = (words.next(), words.next()) {
            if MERMAID_DIRECTIONS.contains(&only.to_ascii_lowercase().as_str()) {
                continue;
            }
        }
        if line.trim().is_empty() {
            continue;
        }

        for arrow in MERMAID_ARROWS {
            line = line.replace(arrow, " ");
        }
        // Edge labels sit between pipes (`--|Yes|-->`); the arrows around them
        // are already gone, so only the delimiters themselves need removing.
        line = line.replace('|', " ");
        // Bracket/paren/brace/quote characters are syntax; what they enclose
        // is a label, kept in place by not touching it here.
        line = line.replace(['[', ']', '(', ')', '{', '}', '"'], " ");

        let cleaned = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !cleaned.is_empty() {
            lines.push(cleaned);
        }
    }

    lines.join("\n")
}

/// Whether the local topic-drift heuristic below may insert extra breaks
/// inside an otherwise-unbroken passage.
///
/// Off until it has been run against a real vault and its false-positive rate
/// observed. Headings already capture nearly all the topic shifts a person
/// bothers to make explicit in a personal notes vault; this exists for the
/// notes that have none, or whose single heading turns out to cover more
/// ground than expected. Shipped disabled rather than half-built: complete and
/// tested, waiting on evidence before it changes anyone's real output. See
/// `docker-compose.local-auth.yml`'s example deployment for how to validate a
/// threshold against real notes before flipping this on.
const TOPIC_DRIFT_ENABLED: bool = false;

/// Jaccard overlap of significant words below which two adjacent paragraphs
/// are different enough topics to split. A placeholder pending validation
/// against a real vault, not a measured number the way `min_score` in
/// `config.rs` is — see the module doc above.
const TOPIC_DRIFT_MIN_OVERLAP: f64 = 0.12;

/// Inside a heading-scoped passage, the heuristic only starts looking once the
/// passage has grown past this fraction of `max_chars` — plausibly long enough
/// to have drifted onto a second topic. A short passage under a heading is
/// almost never that; the heading already said what it's about.
const TOPIC_DRIFT_GATE_FRACTION: f64 = 0.5;

/// A short, common-word list excluded from the overlap comparison, so two
/// paragraphs don't look related purely for sharing "the" and "and".
const STOPWORDS: &[&str] = &[
    "the", "and", "a", "an", "of", "to", "in", "on", "for", "is", "it", "its", "this", "that",
    "with", "as", "at", "by", "or", "be", "are", "was", "were", "i",
];

/// Lowercased, stopword-filtered significant words in a passage, as a set —
/// order and repetition don't matter for an overlap comparison, only which
/// words are present at all.
fn word_set(text: &str) -> std::collections::HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.chars().count() >= 3 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// `|intersection| / |union|`. An empty side means nothing to compare, and
/// that must never be *why* two paragraphs are judged to have drifted apart —
/// a bare heading or a short fragment with no significant words of its own
/// should not force a split, so this returns 1.0 (maximal overlap) rather than
/// 0.0 when either side is empty.
fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 1.0;
    }
    let union = a.union(b).count();
    if union == 0 {
        return 1.0;
    }
    a.intersection(b).count() as f64 / union as f64
}

/// Decides whether the paragraph/item that just ended (`current[boundary..]`)
/// belongs in its own passage rather than merged with what came before it.
///
/// Strictly causal: the decision looks only at `current[..boundary]`, text
/// already fully accumulated from earlier in the note, never at anything that
/// arrives later. A later edit can therefore never reach back and change where
/// an earlier boundary landed — the same property
/// `editing_the_end_of_a_note_leaves_earlier_passages_untouched` already
/// requires of the heading and length-cap breaks, extended to this one.
fn drift_boundary(current: &str, boundary: usize, heading: &str, max_chars: usize) -> bool {
    let before = current[..boundary].trim();
    let new_text = current[boundary..].trim();

    // Too little on one side to say anything about a topic change; two short,
    // naturally terse paragraphs are never split on this alone.
    if before.chars().count() < MIN_CHARS || new_text.chars().count() < MIN_CHARS {
        return false;
    }

    let gated = heading.is_empty()
        || before.chars().count() as f64 > max_chars as f64 * TOPIC_DRIFT_GATE_FRACTION;
    if !gated {
        return false;
    }

    jaccard(&word_set(before), &word_set(new_text)) < TOPIC_DRIFT_MIN_OVERLAP
}

/// Splits a note into passages.
///
/// `max_chars` caps a passage, so one enormous section becomes several rather
/// than being truncated — losing the tail of a section silently is worse than
/// splitting it, because nothing downstream can tell it happened.
pub fn chunks(markdown: &str, max_chars: usize) -> Vec<Chunk> {
    chunks_inner(markdown, max_chars, TOPIC_DRIFT_ENABLED)
}

/// `chunks`, with the topic-drift heuristic's on/off state passed explicitly
/// rather than read from the const, so tests can exercise it deterministically
/// without a global flag that would affect every other test in this file.
fn chunks_inner(markdown: &str, max_chars: usize, topic_drift: bool) -> Vec<Chunk> {
    let body = crate::markdown::body_without_frontmatter(markdown);
    let max_chars = max_chars.max(MIN_CHARS);

    let mut out: Vec<Chunk> = Vec::new();
    let mut headings: Vec<(HeadingLevel, String)> = Vec::new();
    let mut current = String::new();
    let mut heading_at_start = String::new();

    // Depth counters, not flags: a code block inside a list inside a quote still
    // has to come back out at the right level, and a heading's text arrives as
    // the same `Text` events as everything else.
    let mut in_code = false;
    let mut in_heading: Option<HeadingLevel> = None;
    let mut heading_text = String::new();

    // Mermaid is the one fenced language whose content survives at all — see
    // the module doc. Buffered separately from `current` rather than reusing
    // it: mixing diagram syntax into the prose length-cap/space-split logic
    // built for paragraphs would be the wrong tool for it.
    let mut in_mermaid = false;
    let mut mermaid_buffer = String::new();

    // Byte offset into `current` where the paragraph/item now being read
    // began, for the topic-drift check at its end. Always a valid char
    // boundary: it is only ever set to `current.len()` (always a boundary, by
    // construction of `String`) or to 0 by the emergency mid-paragraph split
    // below, and nothing shrinks `current` from the front in between.
    let mut paragraph_start: usize = 0;

    let flush = |current: &mut String, heading: &str, out: &mut Vec<Chunk>| {
        let text = current.trim();
        if text.chars().count() >= MIN_CHARS {
            out.push(Chunk {
                ordinal: out.len() as i32,
                heading: heading.to_string(),
                body: text.to_string(),
            });
        }
        current.clear();
    };

    for event in Parser::new_ext(body, crate::markdown::markdown_options()) {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                // A fenced block ends the passage before it, mermaid or not:
                // prose on either side is not one continuous thought just
                // because whatever sits between them isn't fully embedded.
                flush(&mut current, &heading_at_start, &mut out);
                in_code = true;
                in_mermaid = matches!(&kind, CodeBlockKind::Fenced(lang) if is_mermaid_language(lang));
                mermaid_buffer.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                if in_mermaid {
                    // Reuses `flush`'s own `MIN_CHARS` gate, so a diagram with
                    // too little label text to mean anything is dropped the
                    // same way any other short passage already is — no
                    // special-casing needed here for that.
                    let mut extracted = extract_mermaid_labels(&mermaid_buffer);
                    flush(&mut extracted, &heading_at_start, &mut out);
                }
                in_mermaid = false;
            }

            Event::Start(Tag::Heading { level, .. }) => {
                // A heading ends the passage before it: what follows is about
                // something else, which is the whole reason headings exist.
                flush(&mut current, &heading_at_start, &mut out);
                in_heading = Some(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some(level) = in_heading.take() {
                    // Drop any heading at or below this one: `## B` after `## A`
                    // replaces it, and `# C` replaces both.
                    headings.retain(|(existing, _)| *existing < level);
                    let text = heading_text.trim().to_string();
                    if !text.is_empty() {
                        headings.push((level, text));
                    }
                    heading_at_start = heading_path(&headings);
                }
            }

            Event::Start(Tag::Paragraph) | Event::Start(Tag::Item) => {
                if !in_code && in_heading.is_none() {
                    paragraph_start = current.len();
                }
            }

            Event::Text(text) | Event::Code(text) => {
                if in_heading.is_some() {
                    heading_text.push_str(&text);
                } else if in_code {
                    if in_mermaid {
                        mermaid_buffer.push_str(&text);
                    }
                } else {
                    current.push_str(&text);
                    // Last resort. A single paragraph past twice the cap has no
                    // boundary left to break on, and sending it whole risks the
                    // model refusing the request outright — which would stall
                    // the queue on one note nobody can find. Splitting mid-prose
                    // is the lesser damage, and it is done at a space so the
                    // halves are still readable.
                    while current.chars().count() > max_chars * 2 {
                        let head = split_at_space(&current, max_chars);
                        let tail = current.split_off(head);
                        flush(&mut current, &heading_at_start, &mut out);
                        current = tail;
                        // The paragraph now continues from a fresh, shorter
                        // buffer: `paragraph_start`, recorded against the old
                        // (longer) `current`, would otherwise point past the
                        // end of this one. Nothing meaningful came "before"
                        // this remainder within the buffer any more — it was
                        // just flushed as its own passage — so 0 is correct,
                        // not just safe.
                        paragraph_start = 0;
                    }
                }
            }

            // Paragraph and list-item boundaries are where prose can be broken
            // without cutting a sentence in half, so the cap is applied here
            // rather than the moment it is crossed.
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Item) => {
                if !in_code && in_heading.is_none() {
                    if topic_drift
                        && drift_boundary(&current, paragraph_start, &heading_at_start, max_chars)
                    {
                        let tail = current.split_off(paragraph_start);
                        flush(&mut current, &heading_at_start, &mut out);
                        current = tail;
                    }
                    current.push_str("\n\n");
                    if current.chars().count() >= max_chars {
                        flush(&mut current, &heading_at_start, &mut out);
                    }
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if !in_code && in_heading.is_none() {
                    current.push(' ');
                }
            }
            _ => {}
        }
    }

    flush(&mut current, &heading_at_start, &mut out);
    out
}

/// Byte index to cut at: the last space at or before `limit` characters, or the
/// character boundary at `limit` when the text has no spaces at all.
fn split_at_space(text: &str, limit: usize) -> usize {
    let cut = text
        .char_indices()
        .nth(limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text[..cut].rfind(' ').map(|index| index + 1).unwrap_or(cut)
}

fn heading_path(headings: &[(HeadingLevel, String)]) -> String {
    headings
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" > ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bodies(markdown: &str, max: usize) -> Vec<String> {
        chunks(markdown, max).into_iter().map(|c| c.body).collect()
    }

    const LOREM: &str = "This paragraph is deliberately long enough to clear the minimum \
                         length, because a passage shorter than that carries no meaning a \
                         vector could compare against anything.";

    #[test]
    fn a_note_splits_at_its_headings() {
        let markdown = format!("# One\n\n{LOREM}\n\n# Two\n\n{LOREM} Second.\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].heading, "One");
        assert_eq!(found[1].heading, "Two");
        assert_eq!(found[0].ordinal, 0);
        assert_eq!(found[1].ordinal, 1);
    }

    #[test]
    fn nested_headings_become_a_path() {
        let markdown = format!("# Projects\n\n## Kitchen\n\n{LOREM}\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(found[0].heading, "Projects > Kitchen");
    }

    /// A sibling heading replaces its predecessor rather than accumulating, or
    /// the path grows without bound down a long note.
    #[test]
    fn a_sibling_heading_replaces_rather_than_nests() {
        let markdown =
            format!("# A\n\n## One\n\n{LOREM}\n\n## Two\n\n{LOREM} x\n\n# B\n\n{LOREM} y\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(found[0].heading, "A > One");
        assert_eq!(found[1].heading, "A > Two");
        assert_eq!(found[2].heading, "B");
    }

    #[test]
    fn the_heading_is_prefixed_to_what_gets_embedded_but_not_to_what_is_stored() {
        let markdown = format!("# Kitchen\n\n{LOREM}\n");
        let found = chunks(&markdown, 2000);
        assert!(!found[0].body.contains("Kitchen"));
        assert!(found[0].embedding_text().starts_with("Kitchen\n\n"));
    }

    /// Fenced code is excluded: two notes that both quote a `docker run` line
    /// are not thereby related, and linking them would be noise in the graph.
    #[test]
    fn code_blocks_are_not_embedded() {
        let markdown = format!("{LOREM}\n\n```sh\ndocker run --rm -it alpine sh -lc 'echo hi'\n```\n");
        let found = bodies(&markdown, 2000);
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("docker"));
    }

    #[test]
    fn frontmatter_is_not_embedded() {
        let markdown = format!("---\ntitle: Secret\ntags: [a]\n---\n\n{LOREM}\n");
        let found = bodies(&markdown, 2000);
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("Secret"));
    }

    #[test]
    fn a_passage_too_short_to_mean_anything_is_dropped() {
        assert!(chunks("# H\n\nYes.\n", 2000).is_empty());
        assert!(chunks("", 2000).is_empty());
        assert!(chunks("   \n\n\n", 2000).is_empty());
    }

    /// The ordinary case: a long section made of several paragraphs breaks
    /// between them, so no sentence is ever cut.
    #[test]
    fn an_overlong_section_is_split_at_paragraph_boundaries() {
        let paragraphs = (0..6)
            .map(|i| format!("{LOREM} Paragraph {i}."))
            .collect::<Vec<_>>()
            .join("\n\n");
        let found = chunks(&format!("# H\n\n{paragraphs}\n"), 200);

        assert!(found.len() > 1, "expected several chunks, got {}", found.len());
        assert!(found.iter().all(|c| c.heading == "H"));
        // Nothing is lost, which is the property truncation would break.
        for i in 0..6 {
            assert!(
                found.iter().any(|c| c.body.contains(&format!("Paragraph {i}."))),
                "paragraph {i} went missing"
            );
        }
    }

    /// The pathological case: one paragraph with no boundary to break on. It is
    /// still split, because handing a model a single enormous string gets the
    /// request refused and stalls the queue on one unfindable note.
    #[test]
    fn one_enormous_paragraph_is_split_at_a_space() {
        let found = chunks(&format!("# H\n\n{}\n", LOREM.repeat(8)), 150);
        assert!(found.len() > 1, "expected several chunks, got {}", found.len());
        assert!(found.iter().all(|c| !c.body.starts_with(' ')));
    }

    /// The property the whole embedding cache rests on: editing the end of a
    /// note must leave the earlier passages byte-identical, or every save
    /// re-embeds the whole note and the hash cache buys nothing.
    #[test]
    fn editing_the_end_of_a_note_leaves_earlier_passages_untouched() {
        let before = format!("# A\n\n{LOREM}\n\n# B\n\n{LOREM} second\n");
        let after = format!("# A\n\n{LOREM}\n\n# B\n\n{LOREM} second, now with more\n");

        let a = chunks(&before, 2000);
        let b = chunks(&after, 2000);
        assert_eq!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
    }

    // --- Piece 1: fenced code blocks as hard chunk boundaries -------------

    /// The bug this piece fixes: before the fence flushed the buffer, prose on
    /// either side of a dropped code block was silently spliced into one
    /// passage, as if the block had never been there.
    #[test]
    fn a_code_block_between_two_paragraphs_does_not_stitch_them_into_one_passage() {
        let markdown = format!("# H\n\n{LOREM} First.\n\n```sh\necho hi\n```\n\n{LOREM} Second.\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(found.len(), 2, "the code block must end the passage around it");
        assert!(found[0].body.contains("First."));
        assert!(found[1].body.contains("Second."));
        assert!(!found.iter().any(|c| c.body.contains("echo")));
    }

    #[test]
    fn a_code_block_immediately_after_a_heading_produces_no_spurious_empty_chunk() {
        let markdown = format!("# H\n\n```sh\necho hi\n```\n\n{LOREM}\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].heading, "H");
    }

    // --- Piece 2: mermaid fences as their own embeddable passage ----------

    #[test]
    fn mermaid_language_matches_however_the_fence_was_written() {
        assert!(is_mermaid_language("mermaid"));
        assert!(is_mermaid_language("Mermaid"));
        assert!(is_mermaid_language("MERMAID"));
        assert!(is_mermaid_language("  mermaid  "));
    }

    /// Mirrors `mermaid.test.mjs`'s negative cases exactly: a substring match
    /// here would swallow both.
    #[test]
    fn mermaid_language_is_not_matched_by_near_misses() {
        assert!(!is_mermaid_language(""));
        assert!(!is_mermaid_language("js"));
        assert!(!is_mermaid_language("markdown"));
        assert!(!is_mermaid_language("mermaidjs"));
        assert!(!is_mermaid_language("not-mermaid"));
    }

    #[test]
    fn mermaid_edge_labels_survive_extraction_but_arrows_do_not() {
        let extracted =
            extract_mermaid_labels("graph TD\n    A[Kitchen Service] -->|calls| B[Billing Service]\n");
        assert!(extracted.contains("Kitchen Service"));
        assert!(extracted.contains("Billing Service"));
        assert!(extracted.contains("calls"));
        assert!(!extracted.contains("-->"));
        assert!(!extracted.to_ascii_lowercase().contains("graph"));
    }

    #[test]
    fn mermaid_diagram_type_and_direction_keywords_are_stripped() {
        for src in [
            "graph TD\nA --> B\n",
            "flowchart LR\nA --> B\n",
            "sequenceDiagram\nA->>B: hi\n",
        ] {
            let extracted = extract_mermaid_labels(src).to_ascii_lowercase();
            assert!(!extracted.contains("graph"), "{src}");
            assert!(!extracted.contains("flowchart"), "{src}");
            assert!(!extracted.contains("sequencediagram"), "{src}");
        }
    }

    #[test]
    fn text_on_either_side_of_a_bare_arrow_is_kept_even_without_brackets() {
        let extracted = extract_mermaid_labels("graph TD\nKitchen --> Billing\n");
        assert!(extracted.contains("Kitchen"));
        assert!(extracted.contains("Billing"));
        assert!(!extracted.contains("-->"));
    }

    #[test]
    fn mermaid_edge_label_pipes_are_stripped_but_the_label_text_is_kept() {
        let extracted = extract_mermaid_labels("A --|Yes|--> B\n");
        assert!(extracted.contains("Yes"));
        assert!(!extracted.contains('|'));
        assert!(!extracted.contains("-->"));
    }

    /// Regression guard: piece 2 must not widen inclusion beyond mermaid.
    #[test]
    fn a_non_mermaid_code_block_is_still_fully_excluded() {
        let markdown = format!("{LOREM}\n\n```python\nprint('architecture diagram')\n```\n");
        let found = bodies(&markdown, 2000);
        assert_eq!(found.len(), 1);
        assert!(!found[0].contains("print"));
    }

    #[test]
    fn a_mermaid_diagram_becomes_its_own_chunk_under_the_current_heading() {
        let markdown = format!(
            "# Architecture\n\n{LOREM}\n\n```mermaid\ngraph TD\n    KitchenService[Kitchen Service] --> BillingService[Billing Service]\n    KitchenService --> InventoryService[Inventory Service]\n```\n"
        );
        let found = chunks(&markdown, 2000);
        assert_eq!(found.len(), 2, "prose and diagram become separate passages");
        assert_eq!(found[1].heading, "Architecture");
        assert!(found[1].body.contains("Kitchen Service"));
        assert!(found[1].body.contains("Billing Service"));
        assert!(!found[1].body.contains("-->"));
    }

    #[test]
    fn a_tiny_mermaid_diagram_with_little_label_text_is_dropped_like_any_short_passage() {
        let markdown = format!("# H\n\n{LOREM}\n\n```mermaid\ngraph TD\nA --> B\n```\n");
        let found = chunks(&markdown, 2000);
        assert_eq!(
            found.len(),
            1,
            "the extracted diagram text is too short to mean anything, same as any other short passage"
        );
    }

    #[test]
    fn mermaid_extraction_does_not_disturb_the_ordinals_of_surrounding_prose_chunks() {
        let markdown = format!(
            "# H\n\n{LOREM} First.\n\n```mermaid\ngraph TD\n    KitchenService[Kitchen Service] --> BillingService[Billing Service]\n    KitchenService --> InventoryService[Inventory Service]\n```\n\n{LOREM} Second.\n"
        );
        let found = chunks(&markdown, 2000);
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].ordinal, 0);
        assert_eq!(found[1].ordinal, 1);
        assert_eq!(found[2].ordinal, 2);
        assert!(found[0].body.contains("First."));
        assert!(found[1].body.contains("Kitchen Service"));
        assert!(found[2].body.contains("Second."));
    }

    // --- Piece 3: local topic-drift chunker (shipped disabled) ------------

    const COOKING: &str = "Sourdough starter feeding ratio settled around one to five to five \
                           by weight, twice daily at room temperature, and rye flour speeds up \
                           fermentation noticeably compared to using only all purpose flour for \
                           the same starter.";
    const FINANCE: &str = "Quarterly budget review showed software subscriptions creeping \
                           upward again, mostly forgotten trial periods nobody bothered to \
                           cancel before the renewal charge appeared on the statement at the \
                           end of the month.";

    #[test]
    fn a_long_unheaded_note_that_changes_topic_gets_a_drift_boundary() {
        let markdown = format!("{COOKING}\n\n{FINANCE}\n");
        let found = chunks_inner(&markdown, 2000, true);
        assert_eq!(found.len(), 2, "disjoint vocabulary between adjacent paragraphs should split");
    }

    /// The false-positive case that would fragment ordinary prose and hurt
    /// both retrieval and the cache invariant if the threshold were too
    /// aggressive: two paragraphs about the same thing, sharing most of their
    /// vocabulary and differing only in a couple of words, must stay together
    /// even with no heading to gate on. (A weaker version of this test, with
    /// only three words of real overlap between eighteen and fourteen unique
    /// ones, is exactly the false positive this heuristic must not produce —
    /// real prose about one topic often shares surprisingly few *exact* word
    /// forms without stemming, which is worth remembering before ever turning
    /// `TOPIC_DRIFT_ENABLED` on.)
    #[test]
    fn two_paragraphs_that_repeat_the_same_subject_stay_together() {
        let shared = "Kitchen renovation planning covers the budget, the contractor quotes, \
                      the permit paperwork, and the timeline for finishing the kitchen this \
                      quarter";
        let a = format!("{shared}, starting with the countertop installation.");
        let b = format!("{shared}, starting with the cabinet installation.");
        let markdown = format!("{a}\n\n{b}\n");
        let found = chunks_inner(&markdown, 2000, true);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn topic_drift_detection_never_runs_inside_a_short_heading_scoped_passage() {
        let markdown = format!("# H\n\n{COOKING}\n\n{FINANCE}\n");
        let found = chunks_inner(&markdown, 2000, true);
        assert_eq!(
            found.len(),
            1,
            "a short heading-scoped passage is gated off, regardless of vocabulary drift"
        );
    }

    #[test]
    fn topic_drift_detection_is_off_by_default() {
        let markdown = format!("{COOKING}\n\n{FINANCE}\n");
        assert_eq!(chunks(&markdown, 2000), chunks_inner(&markdown, 2000, false));
        assert_eq!(
            chunks(&markdown, 2000).len(),
            1,
            "the public entry point does not drift-split even though the text would qualify"
        );
    }

    /// The property the whole embedding cache rests on, extended through the
    /// drift path specifically: this is the test that would catch a version of
    /// the heuristic that looked past the paragraph it's deciding about, since
    /// a lookahead is exactly what would let a later edit reach back and
    /// change where an earlier boundary landed.
    #[test]
    fn editing_the_end_of_a_drift_split_note_leaves_earlier_drift_chunks_untouched() {
        let before = format!("{COOKING}\n\n{FINANCE}\n");
        let after =
            format!("{COOKING}\n\n{FINANCE} Also the emergency fund moved to a higher yield account.\n");

        let a = chunks_inner(&before, 2000, true);
        let b = chunks_inner(&after, 2000, true);
        assert_eq!(a[0], b[0]);
        assert_ne!(a[1], b[1]);
    }
}
