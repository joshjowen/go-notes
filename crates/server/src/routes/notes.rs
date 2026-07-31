//! Reading, writing, moving and deleting notes.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use go_notes_shared::{
    Backlink, CreateNoteRequest, MoveRequest, MoveResponse, NoteMeta, NoteResponse, OutgoingLink,
    SaveNoteRequest, SaveNoteResponse,
};
use sqlx::Row;
use uuid::Uuid;

use crate::auth::session::CurrentUser;
use crate::db::User;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::vault::store::NoteFile;
use crate::vault::{index, store, Vault, VaultPath};
use crate::web;

pub async fn read(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
) -> AppResult<Json<NoteResponse>> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_note(&rel_path)?;
    let file = store::read_note(&path).await?;

    // Indexing on read covers the gap between a file appearing on disk and the
    // watcher noticing it — opening a note the user just created over SSH should
    // not show them an empty backlinks pane.
    let note_id = index::index_note_content(&state.pool, &user, &vault, &path, &file).await?;

    let meta = load_meta(&state, &user, &path, &file).await?;
    Ok(Json(NoteResponse {
        meta,
        markdown: file.markdown,
        backlinks: backlinks_for(&state, note_id).await?,
        outgoing: outgoing_for(&state, note_id).await?,
    }))
}

pub async fn save(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
    headers: HeaderMap,
    Json(body): Json<SaveNoteRequest>,
) -> AppResult<Json<SaveNoteResponse>> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_note(&rel_path)?;

    // Optimistic concurrency. Without this, two tabs open on the same note — or
    // a note edited over SSH while the browser tab sat open — would silently
    // lose one side's work, which is the single most damaging thing a notes app
    // can do.
    if store::exists(&path).await {
        let current = store::read_note(&path).await?;
        let expected = headers
            .get(go_notes_shared::IF_MATCH_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.trim_matches('"'));

        match expected {
            None => {
                return Err(AppError::bad_request(
                    "saving an existing note requires an If-Match header",
                ))
            }
            Some(hash) if hash != current.content_hash => {
                return Err(AppError::Conflict {
                    current_markdown: current.markdown,
                    current_hash: current.content_hash,
                })
            }
            Some(_) => {}
        }
    }

    let file = store::write_note(&path, &body.markdown).await?;
    index::index_note_content(&state.pool, &user, &vault, &path, &file).await?;

    let meta = load_meta(&state, &user, &path, &file).await?;
    Ok(Json(SaveNoteResponse { meta }))
}

pub async fn create(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<CreateNoteRequest>,
) -> AppResult<Json<SaveNoteResponse>> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_note(&body.path)?;

    let file = store::create_note(&path, &body.markdown).await?;
    index::index_note_content(&state.pool, &user, &vault, &path, &file).await?;

    tracing::info!(user = %user.username, path = %path.rel(), "created note");
    let meta = load_meta(&state, &user, &path, &file).await?;
    Ok(Json(SaveNoteResponse { meta }))
}

pub async fn delete(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
) -> AppResult<Response> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_note(&rel_path)?;

    // Filesystem first, index second. If the process dies in between, the
    // startup reconcile notices the missing file and cleans up the row — the
    // reverse order would leave an orphaned note the user can no longer see.
    let trash_path = store::move_to_trash(&vault, &path).await?;
    index::remove_note(&state.pool, user.id, path.rel()).await?;

    tracing::info!(user = %user.username, path = %path.rel(), trash = %trash_path, "deleted note");
    Ok(web::no_content())
}

/// Moves or renames a note, following every link that pointed at it.
///
/// This is the operation that makes a vault safe to reorganise. Obsidian users
/// expect a rename to be invisible to the rest of their notes, and a tool that
/// silently breaks fifty links when a file is renamed is one people stop
/// trusting with their writing.
pub async fn move_note(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<MoveRequest>,
) -> AppResult<Json<MoveResponse>> {
    let vault = state.vault_for(&user)?;
    let from = vault.resolve_note(&body.from)?;
    let to = vault.resolve_note(&body.to)?;

    if from.rel() == to.rel() {
        return Ok(Json(MoveResponse {
            to: to.rel().to_string(),
            links_rewritten: 0,
        }));
    }

    store::move_entry(&from, &to).await?;
    index::rename_note_row(&state.pool, user.id, from.rel(), to.rel()).await?;

    // Runs after the rename so the rewritten links resolve to a file that is
    // already in its new place.
    let links_rewritten =
        index::rewrite_links_after_move(&state.pool, &user, &vault, from.rel(), to.rel()).await?;

    tracing::info!(
        user = %user.username,
        from = %from.rel(),
        to = %to.rel(),
        links_rewritten,
        "moved note"
    );
    Ok(Json(MoveResponse {
        to: to.rel().to_string(),
        links_rewritten,
    }))
}

/// Builds the metadata block returned alongside a note.
async fn load_meta(
    state: &AppState,
    user: &User,
    path: &VaultPath,
    file: &NoteFile,
) -> AppResult<NoteMeta> {
    let row = sqlx::query(
        "SELECT n.title,
                COALESCE(
                    (SELECT array_agg(t.name ORDER BY t.name)
                     FROM note_tags nt JOIN tags t ON t.id = nt.tag_id
                     WHERE nt.note_id = n.id),
                    ARRAY[]::text[]
                ) AS tags
         FROM notes n
         WHERE n.user_id = $1 AND n.rel_path = $2",
    )
    .bind(user.id)
    .bind(path.rel())
    .fetch_optional(&state.pool)
    .await?;

    let (title, tags) = match row {
        Some(row) => (
            row.try_get::<String, _>("title")?,
            row.try_get::<Vec<String>, _>("tags")?,
        ),
        // Should not happen — every caller indexes first — but falling back to
        // the filename is better than failing the request.
        None => (path.stem().to_string(), Vec::new()),
    };

    Ok(NoteMeta {
        path: path.rel().to_string(),
        title,
        content_hash: file.content_hash.clone(),
        modified: file.mtime,
        size_bytes: file.size_bytes,
        tags,
    })
}

/// Notes that link *to* this one.
pub async fn backlinks_for(state: &AppState, note_id: Uuid) -> AppResult<Vec<Backlink>> {
    let rows = sqlx::query(
        "SELECT n.rel_path, n.title, l.context
         FROM links l
         JOIN notes n ON n.id = l.source_note_id
         WHERE l.target_note_id = $1
           AND l.source_note_id <> $1
         ORDER BY n.title, l.ordinal
         LIMIT 500",
    )
    .bind(note_id)
    .fetch_all(&state.pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(Backlink {
                path: row.try_get("rel_path")?,
                title: row.try_get("title")?,
                context: row.try_get("context")?,
            })
        })
        .collect()
}

/// Links this note makes, resolved where possible.
async fn outgoing_for(state: &AppState, note_id: Uuid) -> AppResult<Vec<OutgoingLink>> {
    let rows = sqlx::query(
        "SELECT l.target_raw, n.rel_path
         FROM links l
         LEFT JOIN notes n ON n.id = l.target_note_id
         WHERE l.source_note_id = $1
         ORDER BY l.ordinal",
    )
    .bind(note_id)
    .fetch_all(&state.pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(OutgoingLink {
                target_raw: row.try_get("target_raw")?,
                resolved_path: row.try_get("rel_path")?,
            })
        })
        .collect()
}

/// Backlinks for a note addressed by path, for the side panel.
pub async fn backlinks(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
) -> AppResult<Json<Vec<Backlink>>> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_note(&rel_path)?;
    let note_id = note_id_for(&state, &user, &vault, &path).await?;
    Ok(Json(backlinks_for(&state, note_id).await?))
}

/// Looks up a note's id, indexing it first if it is not in the database yet.
async fn note_id_for(
    state: &AppState,
    user: &User,
    vault: &Vault,
    path: &VaultPath,
) -> AppResult<Uuid> {
    let existing: Option<Uuid> =
        sqlx::query("SELECT id FROM notes WHERE user_id = $1 AND rel_path = $2")
            .bind(user.id)
            .bind(path.rel())
            .fetch_optional(&state.pool)
            .await?
            .map(|row| row.try_get("id"))
            .transpose()?;

    match existing {
        Some(id) => Ok(id),
        None => index::index_note(&state.pool, user, vault, path).await,
    }
}
