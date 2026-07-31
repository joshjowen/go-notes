-- The derived index over each user's markdown files.
--
-- Every table here is a cache. The markdown on disk is the source of truth, and
-- `TRUNCATE notes, links, tags, note_tags, attachments` followed by a restart
-- rebuilds all of it. Nothing in here may hold information that does not exist
-- in the files, with the single exception of `folders.collapsed`, which is
-- per-user sidebar state rather than a property of the filesystem.

CREATE TABLE notes (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id      uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    -- Vault-relative, '/'-separated, always ending in .md
    rel_path     text        NOT NULL,
    -- Frontmatter `title:` when present, otherwise the filename stem.
    title        text        NOT NULL,
    -- Filename stem, kept separately because wikilink resolution matches on the
    -- filename regardless of any title the frontmatter declares.
    stem         text        NOT NULL,
    -- Markdown rendered down to plain text, for full-text search only.
    body_text    text        NOT NULL DEFAULT '',
    -- blake3 of the exact bytes on disk. Doubles as the If-Match token for
    -- optimistic concurrency and as the "did this actually change?" check that
    -- makes reindexing idempotent.
    content_hash text        NOT NULL,
    frontmatter  jsonb       NOT NULL DEFAULT '{}'::jsonb,
    mtime        timestamptz NOT NULL,
    size_bytes   bigint      NOT NULL,
    indexed_at   timestamptz NOT NULL DEFAULT now(),

    search tsvector GENERATED ALWAYS AS (
        setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
        setweight(to_tsvector('english', coalesce(body_text, '')), 'B')
    ) STORED,

    UNIQUE (user_id, rel_path)
);

CREATE INDEX notes_search_idx ON notes USING gin (search);
-- Trigram index for the quick switcher, which matches on substrings and typos
-- rather than on whole words the way the tsvector does.
CREATE INDEX notes_title_trgm_idx ON notes USING gin (title gin_trgm_ops);
CREATE INDEX notes_stem_trgm_idx ON notes USING gin (stem gin_trgm_ops);
-- Wikilink resolution by filename, case-insensitively.
CREATE INDEX notes_user_stem_idx ON notes (user_id, lower(stem));
-- Listing a folder's contents.
CREATE INDEX notes_user_path_idx ON notes (user_id, rel_path text_pattern_ops);

CREATE TABLE folders (
    id         uuid    PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    rel_path   text    NOT NULL,
    collapsed  boolean NOT NULL DEFAULT false,
    sort_order integer NOT NULL DEFAULT 0,

    UNIQUE (user_id, rel_path)
);

CREATE TABLE links (
    id             bigserial PRIMARY KEY,
    user_id        uuid    NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    source_note_id uuid    NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    -- NULL means the link is broken: it names something with no file behind it.
    -- ON DELETE SET NULL is what makes a link go red when its target is deleted,
    -- and what lets it heal automatically when the target comes back.
    target_note_id uuid    REFERENCES notes (id) ON DELETE SET NULL,
    -- The target exactly as written in the note, e.g. `Projects/Kitchen Reno`.
    target_raw     text    NOT NULL,
    -- Lowercased lookup key, so resolution is a plain index probe.
    target_key     text    NOT NULL,
    -- `[[Note#Heading|Alias]]` decomposed.
    anchor         text,
    alias          text,
    link_kind      text    NOT NULL CHECK (link_kind IN ('wikilink', 'embed', 'markdown')),
    -- Position of this link within its source note, so rewrites stay ordered.
    ordinal        integer NOT NULL,
    -- Surrounding text, shown in the backlinks pane.
    context        text    NOT NULL DEFAULT ''
);

CREATE INDEX links_source_idx ON links (source_note_id);
CREATE INDEX links_target_idx ON links (user_id, target_note_id);
-- Probed when a note is created or renamed, to adopt links that were previously
-- broken and now point at something real.
CREATE INDEX links_target_key_idx ON links (user_id, target_key);

-- The rule for turning a link target into a note, expressed once.
--
-- Two candidate forms are accepted, mirroring how Obsidian resolves links:
-- a full vault-relative path without its extension (`[[Projects/Budget]]`), or
-- a bare filename (`[[Budget]]`) which may match notes in several folders.
--
-- A full-path match always wins. Among filename matches the shortest path wins,
-- with the path itself as a final tiebreak, so resolution is deterministic:
-- two notes sharing a name will not cause a link to flip between them from one
-- request to the next.
CREATE FUNCTION resolve_link_target(p_user_id uuid, p_target_key text)
RETURNS uuid
LANGUAGE sql
STABLE
AS $$
    SELECT n.id
    FROM notes n
    WHERE n.user_id = p_user_id
      AND (
        lower(left(n.rel_path, length(n.rel_path) - 3)) = p_target_key
        OR (strpos(p_target_key, '/') = 0 AND lower(n.stem) = p_target_key)
      )
    ORDER BY (lower(left(n.rel_path, length(n.rel_path) - 3)) = p_target_key) DESC,
             length(n.rel_path) ASC,
             n.rel_path ASC
    LIMIT 1
$$;

CREATE TABLE tags (
    id      bigserial PRIMARY KEY,
    user_id uuid      NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    name    text      NOT NULL,

    UNIQUE (user_id, name)
);

CREATE TABLE note_tags (
    note_id uuid   NOT NULL REFERENCES notes (id) ON DELETE CASCADE,
    tag_id  bigint NOT NULL REFERENCES tags (id) ON DELETE CASCADE,

    PRIMARY KEY (note_id, tag_id)
);

CREATE INDEX note_tags_tag_idx ON note_tags (tag_id);

CREATE TABLE attachments (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    uuid        NOT NULL REFERENCES users (id) ON DELETE CASCADE,
    rel_path   text        NOT NULL,
    -- Sniffed from the file's contents, never taken from the upload's headers.
    mime       text        NOT NULL,
    size_bytes bigint      NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),

    UNIQUE (user_id, rel_path)
);
