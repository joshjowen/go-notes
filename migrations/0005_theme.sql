-- A user's theme choice and any customisation of it.
--
-- Not derivable from anything on disk or from another table, in the same
-- sense `folders.collapsed` is not (see its comment in 0002): this is a
-- property of this user's view of the app, not of the vault, so the database
-- is the only place it can live. Losing it is a real (if small) loss, unlike
-- everything in the truncate-and-rebuild lists in 0002 and 0004.

CREATE TABLE user_theme (
    user_id       uuid        PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    theme_id      text        NOT NULL,
    -- The custom palette, as the same JSON the browser already keeps in
    -- localStorage. Opaque to the server; meaningful only when
    -- theme_id = 'custom'. NULL rather than duplicating a built-in's fixed
    -- colours into every row that isn't using them.
    custom_colors text,
    custom_css    text        NOT NULL DEFAULT '',
    updated_at    timestamptz NOT NULL DEFAULT now()
);
