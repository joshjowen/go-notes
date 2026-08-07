//! Semantic links: passages, their embeddings, and the edges between notes.
//!
//! The split that makes this work is that **chunking is part of indexing and
//! embedding is not**. Saving a note writes its passages in the same transaction
//! as its links and tags, synchronously, with no network involved; a background
//! worker then notices passages that have no vector yet and fills them in.
//!
//! That is what makes an edit made offline behave correctly with no code of its
//! own. A queued save replays through the ordinary `PUT /api/notes/{path}`
//! handler when the device reconnects, which calls `index_note_content`, which
//! writes the passages — indistinguishable from a save made online. The worker
//! picks them up on its next pass. There is deliberately no offline-specific
//! path here, because a second way in is a second thing to get wrong.
//!
//! It is also why a slow or missing model can never make the application slow:
//! nothing on a request path waits for it. The worst a broken endpoint does is
//! leave the semantic edges stale, which the graph simply does not draw.

pub mod client;
pub mod similarity;
pub mod worker;

pub use client::EmbeddingClient;
