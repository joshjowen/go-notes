-- Semantic links: passages, their embeddings, and the edges derived from them.
--
-- These three join the truncate-and-rebuild list in the header of 0002, which is
-- not amended to say so: sqlx checksums applied migrations, and editing one that
-- has already run anywhere turns every upgrade into "migration 2 was previously
-- applied but has been modified". A comment is not worth that.
--
-- All three tables are caches, in the same sense as everything in 0002. The
-- passages come from the files, the vectors come from the passages, and the
-- edges come from the vectors, so `TRUNCATE note_chunks, embeddings,
-- semantic_links` followed by `go-notes embed --all` rebuilds the lot. The only
-- thing that cannot be rebuilt for free is the money or the minutes spent
-- calling the model, which is exactly why `embeddings` is keyed by content hash
-- rather than by row: editing one paragraph re-embeds one paragraph.
--
-- Deliberately no pgvector. Every deployment in this repository runs stock
-- postgres:17-alpine, which does not ship it, and requiring an extension would
-- break every existing install on upgrade for a similarity search that is a
-- few milliseconds of dot products at the vault sizes this is built for.

CREATE TABLE note_chunks (
    id       bigserial PRIMARY KEY,
    user_id  uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    note_id  uuid    NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    -- Position within the note, from zero.
    ordinal  integer NOT NULL,
    -- The heading path this passage sat under, for showing why two notes matched.
    heading  text    NOT NULL DEFAULT '',
    body     text    NOT NULL,
    -- blake3 of the text actually sent to the model, including the heading path.
    -- The join key into `embeddings`, and the reason an unchanged passage is
    -- never embedded twice.
    body_hash text   NOT NULL,

    UNIQUE (note_id, ordinal)
);

CREATE INDEX note_chunks_note_idx ON note_chunks (note_id);
CREATE INDEX note_chunks_hash_idx ON note_chunks (user_id, body_hash);

CREATE TABLE embeddings (
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Vectors from different models are not comparable, so the model is part of
    -- the key rather than a column: changing models leaves the old rows alone
    -- and simply misses the cache, instead of silently mixing coordinate spaces.
    model      text        NOT NULL,
    body_hash  text        NOT NULL,
    dims       integer     NOT NULL,
    -- dims × 4 bytes, little-endian f32, L2-normalised on the way in so that
    -- cosine similarity is a plain dot product at query time.
    vector     bytea       NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (user_id, model, body_hash)
);

CREATE TABLE semantic_links (
    user_id        uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    source_note_id uuid        NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    target_note_id uuid        NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    -- Cosine similarity of the best-matching pair of passages, in 0..1.
    score          real        NOT NULL,
    -- Which passages matched, so the graph can say why rather than only that.
    source_ordinal integer     NOT NULL,
    target_ordinal integer     NOT NULL,
    computed_at    timestamptz NOT NULL DEFAULT now(),

    PRIMARY KEY (source_note_id, target_note_id)
);

CREATE INDEX semantic_links_user_idx   ON semantic_links (user_id);
CREATE INDEX semantic_links_target_idx ON semantic_links (target_note_id);
