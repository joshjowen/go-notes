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
//! Two things are deliberately excluded. Code blocks, because a fenced shell
//! transcript is similar to every other fenced shell transcript and would link
//! unrelated notes through their tooling. Frontmatter, because tags and titles
//! are already indexed as themselves.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

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

/// Splits a note into passages.
///
/// `max_chars` caps a passage, so one enormous section becomes several rather
/// than being truncated — losing the tail of a section silently is worse than
/// splitting it, because nothing downstream can tell it happened.
pub fn chunks(markdown: &str, max_chars: usize) -> Vec<Chunk> {
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
            Event::Start(Tag::CodeBlock(_)) => in_code = true,
            Event::End(TagEnd::CodeBlock) => in_code = false,

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

            Event::Text(text) | Event::Code(text) => {
                if in_heading.is_some() {
                    heading_text.push_str(&text);
                } else if !in_code {
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
                    }
                }
            }

            // Paragraph and list-item boundaries are where prose can be broken
            // without cutting a sentence in half, so the cap is applied here
            // rather than the moment it is crossed.
            Event::End(TagEnd::Paragraph) | Event::End(TagEnd::Item) => {
                if !in_code && in_heading.is_none() {
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
}
