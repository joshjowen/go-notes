-- Typed links: `[[contradicts::Kitchen Reno]]`.
--
-- A relation is the author's own word for why one note points at another. It is
-- read out of the file like everything else in this schema, so this column is a
-- cache in exactly the way the rest of `links` is, and the `TRUNCATE` in the
-- header of 0002 still rebuilds it.
--
-- Nullable rather than defaulted to '': an ordinary `[[link]]` does not have a
-- relation, and "no relation" is a different thing from "a relation that is the
-- empty string". The graph tells them apart to decide how to draw the edge.

ALTER TABLE links ADD COLUMN relation text;

-- Partial, because the overwhelming majority of links are untyped and there is
-- no reason to carry them in an index that exists to answer "which links say
-- something about their relationship".
CREATE INDEX links_relation_idx ON links (user_id, relation) WHERE relation IS NOT NULL;
