//! The signed-in user's theme choice.
//!
//! One row per user, upserted wholesale rather than patched field by field —
//! the frontend always holds the complete preference in three signals and
//! sends all three together, so there is never a partial value to merge.

use axum::extract::State;
use axum::response::Response;
use axum::Json;
use go_notes_shared::ThemePreference;
use sqlx::Row;

use crate::auth::session::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;
use crate::web;

pub async fn get_theme(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> AppResult<Json<ThemePreference>> {
    let row = sqlx::query(
        "SELECT theme_id, custom_colors, custom_css FROM user_theme WHERE user_id = $1",
    )
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;

    let preference = match row {
        Some(row) => ThemePreference {
            theme_id: row.try_get("theme_id")?,
            custom_colors: row.try_get("custom_colors")?,
            custom_css: row.try_get("custom_css")?,
        },
        // Nothing saved yet — not an error, the frontend already has a sensible
        // default (or last-seen colours in localStorage) to fall back to.
        None => ThemePreference::default(),
    };

    Ok(Json(preference))
}

pub async fn set_theme(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<ThemePreference>,
) -> AppResult<Response> {
    sqlx::query(
        "INSERT INTO user_theme (user_id, theme_id, custom_colors, custom_css)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id) DO UPDATE
         SET theme_id = EXCLUDED.theme_id,
             custom_colors = EXCLUDED.custom_colors,
             custom_css = EXCLUDED.custom_css,
             updated_at = now()",
    )
    .bind(user.id)
    .bind(&body.theme_id)
    .bind(&body.custom_colors)
    .bind(&body.custom_css)
    .execute(&state.pool)
    .await?;

    Ok(web::no_content())
}
