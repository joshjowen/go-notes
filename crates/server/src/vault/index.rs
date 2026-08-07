//! Mirroring the filesystem into Postgres.
//!
//! Everything this module writes is derived: given the markdown files, it can
//! always be recomputed. That makes indexing *idempotent by construction*, which
//! is what lets the same code path serve three different callers — a save from
//! the browser, a change spotted by the watcher, and the startup reconcile —
//! without any of them needing to know about the others.
//!
//! It is also why there is no echo-suppression bookkeeping. When the app writes
//! a note, the watcher sees the write and asks for a reindex; the content hash
//! already matches, so the reindex is a single cheap comparison and stops.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use go_notes_shared::paths;
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::markdown::{self, ParsedLink};
use crate::vault::store::{self, NoteFile};
use crate::vault::{Vault, VaultPath};

// The rule for turning a link target into a note lives in SQL, as the
// `resolve_link_target` function defined in `migrations/0002_vault_index.sql`.
// Keeping it there rather than as a Rust string means the two call sites below
// cannot drift apart, and it keeps every query in this module a static string —
// which sqlx requires unless the caller asserts otherwise.

/// Postgres stores `timestamptz` at microsecond precision, but a filesystem
/// mtime carries nanoseconds. Comparing them directly would report every note as
/// changed on every scan, so both sides are truncated before comparison.
fn truncate_micros(ts: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(ts.timestamp_micros()).unwrap_or(ts)
}

/// What the database currently believes about a note.
#[derive(Debug, Clone)]
pub struct IndexedNote {
    pub id: Uuid,
    pub rel_path: String,
    pub content_hash: String,
    pub mtime: DateTime<Utc>,
    pub size_bytes: i64,
}

/// Reads a note from disk and writes its derived metadata into Postgres.
///
/// Returns the note's id. Safe to call repeatedly.
pub async fn index_note(
    pool: &PgPool,
    user: &User,
    vault: &Vault,
    path: &VaultPath,
) -> AppResult<Uuid> {
    let file = store::read_note(path).await?;
    index_note_content(pool, user, vault, path, &file).await
}

/// Indexes a note whose contents the caller already has, avoiding a second read
/// straight after a save.
pub async fn index_note_content(
    pool: &PgPool,
    user: &User,
    _vault: &Vault,
    path: &VaultPath,
    file: &NoteFile,
) -> AppResult<Uuid> {
    let parsed = markdown::parse(path.stem(), &file.markdown);

    let mut tx = pool.begin().await?;

    let note_id: Uuid = sqlx::query(
        "INSERT INTO notes (user_id, rel_path, title, stem, body_text, content_hash,
                            frontmatter, mtime, size_bytes, indexed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, now())
         ON CONFLICT (user_id, rel_path) DO UPDATE SET
             title        = EXCLUDED.title,
             stem         = EXCLUDED.stem,
             body_text    = EXCLUDED.body_text,
             content_hash = EXCLUDED.content_hash,
             frontmatter  = EXCLUDED.frontmatter,
             mtime        = EXCLUDED.mtime,
             size_bytes   = EXCLUDED.size_bytes,
             indexed_at   = now()
         RETURNING id",
    )
    .bind(user.id)
    .bind(path.rel())
    .bind(&parsed.title)
    .bind(&parsed.stem)
    .bind(&parsed.body_text)
    .bind(&file.content_hash)
    .bind(&parsed.frontmatter)
    .bind(truncate_micros(file.mtime))
    .bind(file.size_bytes)
    .fetch_one(&mut *tx)
    .await?
    .try_get("id")?;

    replace_links(&mut tx, user.id, note_id, &parsed.links).await?;
    replace_tags(&mut tx, user.id, note_id, &parsed.tags).await?;
    replace_chunks(&mut tx, user.id, note_id, &file.markdown).await?;

    tx.commit().await?;

    // Resolution runs outside the transaction: it touches links belonging to
    // other notes, and holding row locks on them for the duration of a save
    // would serialise unrelated edits against each other.
    resolve_outgoing(pool, note_id).await?;
    reresolve_keys(pool, user.id, &keys_for(path.rel(), path.stem())).await?;

    Ok(note_id)
}

/// Indexes only if the file has actually changed since the last pass.
///
/// This is the fast path for the watcher and the startup scan. Returns `true`
/// when work was done.
pub async fn index_if_changed(
    pool: &PgPool,
    user: &User,
    vault: &Vault,
    path: &VaultPath,
) -> AppResult<bool> {
    let file = store::read_note(path).await?;

    let existing: Option<String> =
        sqlx::query("SELECT content_hash FROM notes WHERE user_id = $1 AND rel_path = $2")
            .bind(user.id)
            .bind(path.rel())
            .fetch_optional(pool)
            .await?
            .map(|row| row.try_get("content_hash"))
            .transpose()?;

    if existing.as_deref() == Some(file.content_hash.as_str()) {
        return Ok(false);
    }

    index_note_content(pool, user, vault, path, &file).await?;
    Ok(true)
}

async fn replace_links(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    note_id: Uuid,
    links: &[ParsedLink],
) -> AppResult<()> {
    sqlx::query("DELETE FROM links WHERE source_note_id = $1")
        .bind(note_id)
        .execute(&mut **tx)
        .await?;

    if links.is_empty() {
        return Ok(());
    }

    // One statement with unnested arrays rather than N inserts: a note with a
    // hundred links is common in a linked vault, and a hundred round trips per
    // keystroke-triggered save is not.
    let mut target_raw = Vec::with_capacity(links.len());
    let mut target_key = Vec::with_capacity(links.len());
    let mut anchors = Vec::with_capacity(links.len());
    let mut aliases = Vec::with_capacity(links.len());
    let mut kinds = Vec::with_capacity(links.len());
    let mut ordinals = Vec::with_capacity(links.len());
    let mut contexts = Vec::with_capacity(links.len());
    let mut relations = Vec::with_capacity(links.len());

    for (ordinal, link) in links.iter().enumerate() {
        target_raw.push(link.target_raw.clone());
        target_key.push(link.target_key());
        anchors.push(link.anchor.clone());
        aliases.push(link.alias.clone());
        kinds.push(link.kind.as_str().to_string());
        ordinals.push(ordinal as i32);
        contexts.push(link.context.clone());
        relations.push(link.relation.clone());
    }

    sqlx::query(
        "INSERT INTO links (user_id, source_note_id, target_raw, target_key,
                            anchor, alias, link_kind, ordinal, context, relation)
         SELECT $1, $2, t.raw, t.key, t.anchor, t.alias, t.kind, t.ordinal,
                t.context, t.relation
         FROM unnest($3::text[], $4::text[], $5::text[], $6::text[],
                     $7::text[], $8::int[], $9::text[], $10::text[])
              AS t(raw, key, anchor, alias, kind, ordinal, context, relation)",
    )
    .bind(user_id)
    .bind(note_id)
    .bind(&target_raw)
    .bind(&target_key)
    .bind(&anchors)
    .bind(&aliases)
    .bind(&kinds)
    .bind(&ordinals)
    .bind(&contexts)
    .bind(&relations)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Splits the note into passages and stores them for the embedding worker.
///
/// In the same transaction as the links and the tags, and synchronous, because
/// this is only text handling — no network is involved here and none may be. The
/// model is reached from the background worker, never from a request. That is
/// what stops a slow endpoint from making a save slow, and it is what makes a
/// save replayed from an offline queue behave exactly like any other save: it
/// arrives through the same handler, so the passages are written the same way
/// and the worker picks them up on its next pass. There is deliberately no
/// offline-specific path, because a second way in is a second thing to get wrong.
///
/// Passages are always rewritten in full rather than diffed. The rows are cheap,
/// and the *embeddings* are keyed by content hash anyway — so rewriting a row
/// that says the same thing costs nothing at the model, while diffing would let a
/// note whose paragraphs were reordered leave a stale passage behind.
async fn replace_chunks(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    note_id: Uuid,
    markdown: &str,
) -> AppResult<()> {
    sqlx::query("DELETE FROM note_chunks WHERE note_id = $1")
        .bind(note_id)
        .execute(&mut **tx)
        .await?;

    let chunks = crate::chunk::chunks(markdown, crate::chunk::DEFAULT_CHUNK_CHARS);
    if chunks.is_empty() {
        return Ok(());
    }

    let mut ordinals = Vec::with_capacity(chunks.len());
    let mut headings = Vec::with_capacity(chunks.len());
    let mut bodies = Vec::with_capacity(chunks.len());
    let mut hashes = Vec::with_capacity(chunks.len());

    for chunk in &chunks {
        ordinals.push(chunk.ordinal);
        headings.push(chunk.heading.clone());
        bodies.push(chunk.body.clone());
        // Hashed over exactly what the model will be sent, heading path included:
        // a paragraph moved under a different heading means something else there,
        // and should be embedded again rather than hit the cache.
        hashes.push(blake3::hash(chunk.embedding_text().as_bytes()).to_hex().to_string());
    }

    sqlx::query(
        "INSERT INTO note_chunks (user_id, note_id, ordinal, heading, body, body_hash)
         SELECT $1, $2, t.ordinal, t.heading, t.body, t.hash
         FROM unnest($3::int[], $4::text[], $5::text[], $6::text[])
              AS t(ordinal, heading, body, hash)",
    )
    .bind(user_id)
    .bind(note_id)
    .bind(&ordinals)
    .bind(&headings)
    .bind(&bodies)
    .bind(&hashes)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

async fn replace_tags(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
    note_id: Uuid,
    tags: &[String],
) -> AppResult<()> {
    sqlx::query("DELETE FROM note_tags WHERE note_id = $1")
        .bind(note_id)
        .execute(&mut **tx)
        .await?;

    if !tags.is_empty() {
        // `DO UPDATE` rather than `DO NOTHING` because only the former makes
        // RETURNING yield a row for tags that already existed.
        sqlx::query(
            "WITH inserted AS (
                 INSERT INTO tags (user_id, name)
                 SELECT $1, t FROM unnest($2::text[]) AS t
                 ON CONFLICT (user_id, name) DO UPDATE SET name = EXCLUDED.name
                 RETURNING id
             )
             INSERT INTO note_tags (note_id, tag_id)
             SELECT $3, id FROM inserted
             ON CONFLICT DO NOTHING",
        )
        .bind(user_id)
        .bind(tags)
        .bind(note_id)
        .execute(&mut **tx)
        .await?;
    }

    // Drop tags nothing references any more, so the tag pane does not slowly
    // fill with names the user has stopped using.
    sqlx::query(
        "DELETE FROM tags t
         WHERE t.user_id = $1
           AND NOT EXISTS (SELECT 1 FROM note_tags nt WHERE nt.tag_id = t.id)",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;

    Ok(())
}

/// Points this note's own links at whatever they currently name.
async fn resolve_outgoing(pool: &PgPool, note_id: Uuid) -> AppResult<()> {
    sqlx::query(
        "UPDATE links l SET target_note_id = resolve_link_target(l.user_id, l.target_key)
         WHERE l.source_note_id = $1",
    )
    .bind(note_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Re-resolves every link in the vault that names one of `keys`.
///
/// This is what makes broken links heal. Write `[[Meeting Notes]]` before that
/// note exists and the link is stored unresolved; create the file later and this
/// call — triggered by indexing the new note — adopts the waiting link.
///
/// It also handles the reverse. Deleting a note re-resolves the links that
/// pointed at it, so they either go broken or fall back to another note of the
/// same name, rather than silently keeping a dangling id.
pub async fn reresolve_keys(pool: &PgPool, user_id: Uuid, keys: &[String]) -> AppResult<()> {
    if keys.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE links l SET target_note_id = resolve_link_target(l.user_id, l.target_key)
         WHERE l.user_id = $1 AND l.target_key = ANY($2)",
    )
    .bind(user_id)
    .bind(keys)
    .execute(pool)
    .await?;
    Ok(())
}

/// The link keys a note at `rel_path` can answer to.
pub fn keys_for(rel_path: &str, stem: &str) -> Vec<String> {
    let without_extension = rel_path
        .strip_suffix(".md")
        .or_else(|| rel_path.strip_suffix(".MD"))
        .unwrap_or(rel_path);

    let mut keys = vec![without_extension.to_lowercase(), stem.to_lowercase()];
    keys.sort();
    keys.dedup();
    keys
}

/// Forgets a note. Links pointing at it are re-resolved rather than left dangling.
pub async fn remove_note(pool: &PgPool, user_id: Uuid, rel_path: &str) -> AppResult<()> {
    let keys = keys_for(rel_path, paths::stem(rel_path));

    // `links.target_note_id` is ON DELETE SET NULL, so this breaks inbound links
    // automatically; the re-resolve afterwards gives them a chance to find a
    // different note with the same name.
    sqlx::query("DELETE FROM notes WHERE user_id = $1 AND rel_path = $2")
        .bind(user_id)
        .bind(rel_path)
        .execute(pool)
        .await?;

    reresolve_keys(pool, user_id, &keys).await?;

    sqlx::query(
        "DELETE FROM tags t
         WHERE t.user_id = $1
           AND NOT EXISTS (SELECT 1 FROM note_tags nt WHERE nt.tag_id = t.id)",
    )
    .bind(user_id)
    .execute(pool)
    .await?;

    Ok(())
}

/// Renames a note's row without reparsing it.
///
/// The file's *content* has not changed, only its location, so re-reading and
/// re-parsing it would be wasted work. Its links are unaffected; the links
/// pointing *at* it are handled by re-resolving both the old and new keys.
pub async fn rename_note_row(
    pool: &PgPool,
    user_id: Uuid,
    from: &str,
    to: &str,
) -> AppResult<()> {
    let new_stem = paths::stem(to);

    sqlx::query(
        "UPDATE notes SET rel_path = $3, stem = $4,
                          title = CASE WHEN frontmatter ? 'title' THEN title ELSE $4 END
         WHERE user_id = $1 AND rel_path = $2",
    )
    .bind(user_id)
    .bind(from)
    .bind(to)
    .bind(new_stem)
    .execute(pool)
    .await?;

    let mut keys = keys_for(from, paths::stem(from));
    keys.extend(keys_for(to, new_stem));
    keys.sort();
    keys.dedup();
    reresolve_keys(pool, user_id, &keys).await?;

    Ok(())
}

/// What the database currently holds for a user's vault.
async fn indexed_notes(pool: &PgPool, user_id: Uuid) -> AppResult<HashMap<String, IndexedNote>> {
    let rows = sqlx::query(
        "SELECT id, rel_path, content_hash, mtime, size_bytes FROM notes WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;

    let mut map = HashMap::with_capacity(rows.len());
    for row in rows {
        let note = IndexedNote {
            id: row.try_get("id")?,
            rel_path: row.try_get("rel_path")?,
            content_hash: row.try_get("content_hash")?,
            mtime: row.try_get("mtime")?,
            size_bytes: row.try_get("size_bytes")?,
        };
        map.insert(note.rel_path.clone(), note);
    }
    Ok(map)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub failed: usize,
}

impl ReconcileReport {
    pub fn changed_anything(&self) -> bool {
        self.added + self.updated + self.removed > 0
    }
}

/// Brings the database back in line with what is actually on disk.
///
/// This is the safety net for every way the two can drift: a note edited over
/// SSH, a `git pull`, a crash between the rename and the database update, or a
/// database restored from a backup taken at a different moment. Running it is
/// always safe, and after it runs the index is correct by definition.
pub async fn reconcile_vault(pool: &PgPool, user: &User, vault: &Vault) -> AppResult<ReconcileReport> {
    let mut report = ReconcileReport::default();

    let on_disk = store::scan_notes(vault).await?;
    let mut in_db = indexed_notes(pool, user.id).await?;

    for file in &on_disk {
        let known = in_db.remove(&file.rel_path);

        // Size and mtime are a cheap proxy for "unchanged". Getting this wrong
        // in the conservative direction just means a redundant reparse; getting
        // it wrong the other way would leave a stale index, so any difference
        // at all triggers a reindex.
        let unchanged = known.as_ref().is_some_and(|note| {
            note.size_bytes == file.size_bytes
                && truncate_micros(note.mtime) == truncate_micros(file.mtime)
        });
        if unchanged {
            continue;
        }

        let path = match vault.resolve_note(&file.rel_path) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(path = %file.rel_path, error = %err, "skipping unindexable note");
                report.failed += 1;
                continue;
            }
        };

        match index_note(pool, user, vault, &path).await {
            // A hash match means only the mtime moved — a `touch`, or a restore
            // that preserved content. Nothing changed for the user.
            Ok(_) if known.is_some() => report.updated += 1,
            Ok(_) => report.added += 1,
            Err(err) => {
                tracing::warn!(path = %file.rel_path, error = %err, "failed to index note");
                report.failed += 1;
            }
        }
    }

    // Whatever is left in `in_db` has no file behind it any more.
    for (rel_path, _) in in_db {
        remove_note(pool, user.id, &rel_path).await?;
        report.removed += 1;
    }

    sync_folders(pool, user, vault).await?;
    sync_attachments(pool, user, vault).await?;

    if report.changed_anything() || report.failed > 0 {
        tracing::info!(
            user = %user.username,
            added = report.added,
            updated = report.updated,
            removed = report.removed,
            failed = report.failed,
            "reconciled vault"
        );
    }
    Ok(report)
}

/// Mirrors the directory structure into `folders`.
///
/// The rows exist so that an empty folder still shows in the sidebar — the
/// filesystem is the source of truth for *which* folders exist, but the database
/// is the only place the user's collapse state can live.
async fn sync_folders(pool: &PgPool, user: &User, vault: &Vault) -> AppResult<()> {
    let on_disk = store::scan_folders(vault).await?;

    // Folders whose names the API cannot address would be listed but unusable.
    let addressable: Vec<String> = on_disk
        .into_iter()
        .filter(|rel| paths::validate_rel_path(rel).is_ok())
        .collect();

    sqlx::query(
        "INSERT INTO folders (user_id, rel_path)
         SELECT $1, f FROM unnest($2::text[]) AS f
         ON CONFLICT (user_id, rel_path) DO NOTHING",
    )
    .bind(user.id)
    .bind(&addressable)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM folders WHERE user_id = $1 AND NOT (rel_path = ANY($2))")
        .bind(user.id)
        .bind(&addressable)
        .execute(pool)
        .await?;

    Ok(())
}

async fn sync_attachments(pool: &PgPool, user: &User, vault: &Vault) -> AppResult<()> {
    let on_disk = store::scan_attachments(vault).await?;

    let paths_on_disk: Vec<String> = on_disk.iter().map(|f| f.rel_path.clone()).collect();
    let sizes: Vec<i64> = on_disk.iter().map(|f| f.size_bytes).collect();
    let mimes: Vec<String> = on_disk
        .iter()
        .map(|f| {
            mime_guess::from_path(&f.rel_path)
                .first_or_octet_stream()
                .essence_str()
                .to_string()
        })
        .collect();

    sqlx::query(
        "INSERT INTO attachments (user_id, rel_path, mime, size_bytes)
         SELECT $1, a.path, a.mime, a.size
         FROM unnest($2::text[], $3::text[], $4::bigint[]) AS a(path, mime, size)
         ON CONFLICT (user_id, rel_path)
         DO UPDATE SET size_bytes = EXCLUDED.size_bytes",
    )
    .bind(user.id)
    .bind(&paths_on_disk)
    .bind(&mimes)
    .bind(&sizes)
    .execute(pool)
    .await?;

    sqlx::query("DELETE FROM attachments WHERE user_id = $1 AND NOT (rel_path = ANY($2))")
        .bind(user.id)
        .bind(&paths_on_disk)
        .execute(pool)
        .await?;

    Ok(())
}

/// Reconciles every user's vault. Called once at startup.
pub async fn reconcile_all(pool: &PgPool, data_dir: &std::path::Path) -> AppResult<()> {
    let users = crate::db::list_users(pool).await?;
    tracing::info!(users = users.len(), "reconciling vaults against the filesystem");

    for user in users {
        let vault = match Vault::open(data_dir, &user.vault_dir) {
            Ok(vault) => vault,
            Err(err) => {
                // One broken vault must not stop the server from starting for
                // everybody else.
                tracing::error!(user = %user.username, error = %err, "could not open vault");
                continue;
            }
        };
        if let Err(err) = reconcile_vault(pool, &user, &vault).await {
            tracing::error!(user = %user.username, error = %err, "reconcile failed");
        }
    }
    Ok(())
}

/// Rewrites `[[old]]` references to point at a note's new name.
///
/// Called after a note is renamed or moved. Only the *target* part of each link
/// is replaced: an existing `|alias` and `#anchor` are preserved verbatim, so
/// `[[Old Name#Budget|see the numbers]]` becomes
/// `[[New Name#Budget|see the numbers]]` and the reader sees no change at all.
///
/// Returns the number of notes whose files were modified.
pub async fn rewrite_links_after_move(
    pool: &PgPool,
    user: &User,
    vault: &Vault,
    from_rel: &str,
    to_rel: &str,
) -> AppResult<usize> {
    let old_keys = keys_for(from_rel, paths::stem(from_rel));

    // Every note that referred to the old name, by any of its keys.
    let rows = sqlx::query(
        "SELECT DISTINCT n.rel_path
         FROM links l
         JOIN notes n ON n.id = l.source_note_id
         WHERE l.user_id = $1 AND l.target_key = ANY($2)",
    )
    .bind(user.id)
    .bind(&old_keys)
    .fetch_all(pool)
    .await?;

    let sources: Vec<String> = rows
        .into_iter()
        .map(|row| row.try_get::<String, _>("rel_path"))
        .collect::<Result<_, _>>()?;

    let mut rewritten = 0usize;
    for source_rel in sources {
        // A note that links to itself needs no rewrite of its own body when it
        // is the note being moved — its links move with it.
        let path = match vault.resolve_note(&source_rel) {
            Ok(path) => path,
            Err(err) => {
                tracing::warn!(path = %source_rel, error = %err, "skipping link rewrite");
                continue;
            }
        };

        let file = match store::read_note(&path).await {
            Ok(file) => file,
            Err(err) => {
                tracing::warn!(path = %source_rel, error = %err, "could not read note to rewrite links");
                continue;
            }
        };

        let Some(updated) = rewrite_link_targets(&file.markdown, &old_keys, from_rel, to_rel) else {
            continue;
        };

        let written = store::write_note(&path, &updated).await?;
        index_note_content(pool, user, vault, &path, &written).await?;
        rewritten += 1;
    }

    if rewritten > 0 {
        tracing::info!(from = %from_rel, to = %to_rel, notes = rewritten, "rewrote links after move");
    }
    Ok(rewritten)
}

/// Produces the new body of a note whose links to `from_rel` must now point at
/// `to_rel`, or `None` if nothing needed changing.
///
/// Kept separate from the IO so it can be tested exhaustively — this is the
/// function that edits the user's files, so it is the one that has to be right.
pub fn rewrite_link_targets(
    content: &str,
    old_keys: &[String],
    from_rel: &str,
    to_rel: &str,
) -> Option<String> {
    let parsed = markdown::parse(paths::stem(from_rel), content);

    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for link in &parsed.links {
        if !old_keys.contains(&link.target_key()) {
            continue;
        }

        // Preserve how the author wrote the reference. Someone who writes
        // `[[Budget]]` gets `[[New Budget]]`; someone who writes the full path
        // gets the full new path.
        let wrote_full_path = link.target_raw.contains('/');
        let new_target = if wrote_full_path {
            to_rel.strip_suffix(".md").unwrap_or(to_rel).to_string()
        } else {
            paths::stem(to_rel).to_string()
        };

        let replacement = match link.kind {
            markdown::LinkKind::Wikilink | markdown::LinkKind::Embed
                if content[link.span.clone()].contains("[[") =>
            {
                let prefix = if content[link.span.clone()].starts_with('!') {
                    "!"
                } else {
                    ""
                };
                let anchor = link
                    .anchor
                    .as_ref()
                    .map(|a| format!("#{a}"))
                    .unwrap_or_default();
                let alias = link
                    .alias
                    .as_ref()
                    .map(|a| format!("|{a}"))
                    .unwrap_or_default();
                // The relation is the author's word for the relationship, not
                // part of the address, so a move must carry it across untouched.
                // Dropping it here would quietly delete the only thing that made
                // the link say more than "these two are connected".
                let relation = link
                    .relation
                    .as_ref()
                    .map(|r| format!("{r}::"))
                    .unwrap_or_default();
                format!("{prefix}[[{relation}{new_target}{anchor}{alias}]]")
            }
            // A `[text](target.md)` link: rewrite only the target, leaving the
            // visible text exactly as the author wrote it.
            _ => {
                let original = &content[link.span.clone()];
                let Some(open) = original.rfind('(') else {
                    continue;
                };
                let Some(close) = original.rfind(')') else {
                    continue;
                };
                if close <= open {
                    continue;
                }
                let anchor = link
                    .anchor
                    .as_ref()
                    .map(|a| format!("#{a}"))
                    .unwrap_or_default();
                let encoded = encode_link_target(&format!("{new_target}.md{anchor}"));
                format!("{}({}){}", &original[..open], encoded, &original[close + 1..])
            }
        };

        // A link written as a bare filename usually needs no change when the
        // note moves between folders: `[[Kitchen Reno]]` still names the same
        // note wherever it lives. Skipping identical replacements is what keeps
        // a folder move from rewriting — and bumping the mtime of — every file
        // that merely mentioned something inside it, which would show up as a
        // page of empty diffs in a vault kept under git.
        if content[link.span.clone()] == replacement {
            continue;
        }

        edits.push((link.span.clone(), replacement));
    }

    if edits.is_empty() {
        return None;
    }

    // Apply back to front so earlier spans keep their offsets.
    edits.sort_by_key(|(span, _)| std::cmp::Reverse(span.start));
    let mut out = content.to_string();
    for (span, replacement) in edits {
        out.replace_range(span, &replacement);
    }
    Some(out)
}

/// Percent-encodes the characters that would break a markdown link target.
/// Spaces are the common case; everything else is left readable on purpose.
fn encode_link_target(target: &str) -> String {
    target
        .replace('%', "%25")
        .replace(' ', "%20")
        .replace('(', "%28")
        .replace(')', "%29")
}

/// Collects the ids of notes whose links must be re-resolved when a subtree
/// moves, used by the folder-rename path.
pub async fn notes_under(pool: &PgPool, user_id: Uuid, folder: &str) -> AppResult<Vec<String>> {
    let pattern = if folder.is_empty() {
        "%".to_string()
    } else {
        format!("{folder}/%")
    };
    let rows = sqlx::query(
        "SELECT rel_path FROM notes WHERE user_id = $1 AND rel_path LIKE $2 ORDER BY rel_path",
    )
    .bind(user_id)
    .bind(pattern)
    .fetch_all(pool)
    .await?;
    rows.into_iter()
        .map(|row| row.try_get::<String, _>("rel_path").map_err(AppError::from))
        .collect()
}

/// Notes that exist in the index but whose files vanished — used by tests and
/// by the `reconcile` CLI subcommand to report drift without fixing it.
pub async fn drifted_paths(pool: &PgPool, user: &User, vault: &Vault) -> AppResult<Vec<String>> {
    let on_disk: HashSet<String> = store::scan_notes(vault)
        .await?
        .into_iter()
        .map(|f| f.rel_path)
        .collect();
    let in_db = indexed_notes(pool, user.id).await?;

    let mut drift: Vec<String> = in_db
        .keys()
        .filter(|path| !on_disk.contains(*path))
        .cloned()
        .collect();
    drift.sort();
    Ok(drift)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(from: &str) -> Vec<String> {
        keys_for(from, paths::stem(from))
    }

    #[test]
    fn a_note_answers_to_its_filename_and_its_full_path() {
        assert_eq!(
            keys_for("Projects/Kitchen Reno.md", "Kitchen Reno"),
            vec!["kitchen reno", "projects/kitchen reno"]
        );
        // At the vault root the two forms coincide and must not be duplicated.
        assert_eq!(keys_for("Note.md", "Note"), vec!["note"]);
    }

    #[test]
    fn rewrites_a_bare_wikilink() {
        let out = rewrite_link_targets(
            "See [[Old Name]] for details.\n",
            &keys("Old Name.md"),
            "Old Name.md",
            "New Name.md",
        );
        assert_eq!(out.unwrap(), "See [[New Name]] for details.\n");
    }

    /// A relation is the author's word for the relationship, not part of the
    /// address. Rewriting the address must not take it with it — losing it here
    /// would silently downgrade a typed link to a plain one, in a file edit the
    /// author never made and would only notice in the graph much later.
    #[test]
    fn preserves_the_relation_on_a_typed_link() {
        let out = rewrite_link_targets(
            "This [[contradicts::Old]] and ![[illustrates::Old]].\n",
            &keys("Old.md"),
            "Old.md",
            "New.md",
        );
        assert_eq!(
            out.unwrap(),
            "This [[contradicts::New]] and ![[illustrates::New]].\n"
        );
    }

    #[test]
    fn preserves_a_relation_alongside_an_anchor_and_an_alias() {
        let out = rewrite_link_targets(
            "See [[supersedes::Old#Budget|the numbers]].\n",
            &keys("Old.md"),
            "Old.md",
            "Projects/New.md",
        );
        // The author wrote a bare filename, so they keep a bare filename.
        assert_eq!(out.unwrap(), "See [[supersedes::New#Budget|the numbers]].\n");
    }

    /// The detail that makes a rename invisible to the reader: everything the
    /// author wrote apart from the target itself has to survive untouched.
    #[test]
    fn preserves_anchors_and_aliases() {
        let out = rewrite_link_targets(
            "See [[Old#Budget|the numbers]] and ![[Old]].\n",
            &keys("Old.md"),
            "Old.md",
            "New.md",
        );
        assert_eq!(
            out.unwrap(),
            "See [[New#Budget|the numbers]] and ![[New]].\n"
        );
    }

    /// Someone who wrote a full path meant a full path; someone who wrote a bare
    /// name meant a bare name. A rename should not quietly change their style.
    #[test]
    fn keeps_the_reference_style_the_author_chose() {
        let out = rewrite_link_targets(
            "Bare [[Budget]] and full [[Projects/Budget]].\n",
            &keys("Projects/Budget.md"),
            "Projects/Budget.md",
            "Archive/Old Budget.md",
        );
        assert_eq!(
            out.unwrap(),
            "Bare [[Old Budget]] and full [[Archive/Old Budget]].\n"
        );
    }

    #[test]
    fn rewrites_inline_markdown_links_and_encodes_spaces() {
        let out = rewrite_link_targets(
            "See [the budget](Budget.md) please.\n",
            &keys("Budget.md"),
            "Budget.md",
            "Q3 Budget.md",
        );
        assert_eq!(
            out.unwrap(),
            "See [the budget](Q3%20Budget.md) please.\n"
        );
    }

    /// Moving a note between folders leaves bare-name links spelling exactly
    /// what they spelled before, so the file must not be rewritten at all.
    #[test]
    fn a_folder_move_does_not_touch_files_whose_text_would_not_change() {
        assert_eq!(
            rewrite_link_targets(
                "Back to [[Kitchen Reno]].\n",
                &keys("Projects/Kitchen Reno.md"),
                "Projects/Kitchen Reno.md",
                "Work/Renovations/Kitchen Reno.md",
            ),
            None
        );
    }

    /// But a note in the same file that *did* spell the full path still gets
    /// updated, even though its neighbour did not.
    #[test]
    fn rewrites_only_the_links_that_actually_change() {
        let out = rewrite_link_targets(
            "Full: [[Projects/Kitchen Reno]]\nBare: [[Kitchen Reno]]\n",
            &keys("Projects/Kitchen Reno.md"),
            "Projects/Kitchen Reno.md",
            "Work/Renovations/Kitchen Reno.md",
        );
        assert_eq!(
            out.unwrap(),
            "Full: [[Work/Renovations/Kitchen Reno]]\nBare: [[Kitchen Reno]]\n"
        );
    }

    #[test]
    fn leaves_unrelated_links_alone() {
        let content = "Links to [[Other]] and [[Something Else]].\n";
        assert_eq!(
            rewrite_link_targets(content, &keys("Old.md"), "Old.md", "New.md"),
            None
        );
    }

    /// A link inside a code fence is documentation. Rewriting it would corrupt
    /// a code sample, which is exactly the kind of silent damage that makes
    /// people stop trusting an editor with their files.
    #[test]
    fn never_touches_links_inside_code() {
        let content = "Real [[Old]].\n\n```\n[[Old]]\n```\n\nInline `[[Old]]`.\n";
        let out = rewrite_link_targets(content, &keys("Old.md"), "Old.md", "New.md").unwrap();
        assert_eq!(
            out,
            "Real [[New]].\n\n```\n[[Old]]\n```\n\nInline `[[Old]]`.\n"
        );
    }

    #[test]
    fn rewrites_every_occurrence_in_one_pass() {
        let out = rewrite_link_targets(
            "[[Old]] then [[Old]] then [[Old#a]].\n",
            &keys("Old.md"),
            "Old.md",
            "New.md",
        );
        assert_eq!(out.unwrap(), "[[New]] then [[New]] then [[New#a]].\n");
    }

    /// Multi-byte text before a link means byte offsets and character offsets
    /// diverge; applying edits back-to-front has to keep them aligned.
    #[test]
    fn handles_multibyte_text_around_links() {
        let out = rewrite_link_targets(
            "日本語のテキスト [[Old]] さらに 🎉 [[Old]] 終わり\n",
            &keys("Old.md"),
            "Old.md",
            "New.md",
        );
        assert_eq!(
            out.unwrap(),
            "日本語のテキスト [[New]] さらに 🎉 [[New]] 終わり\n"
        );
    }

    #[test]
    fn rewrites_links_written_with_the_md_extension() {
        let out = rewrite_link_targets(
            "See [[Old.md]].\n",
            &keys("Old.md"),
            "Old.md",
            "New.md",
        );
        assert_eq!(out.unwrap(), "See [[New]].\n");
    }

    #[test]
    fn matching_is_case_insensitive() {
        let out = rewrite_link_targets(
            "See [[old name]] and [[OLD NAME]].\n",
            &keys("Old Name.md"),
            "Old Name.md",
            "New.md",
        );
        assert_eq!(out.unwrap(), "See [[New]] and [[New]].\n");
    }
}
