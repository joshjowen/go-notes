//! Full-text search, the quick switcher, and the tag list.

use axum::extract::{Query, State};
use axum::Json;
use go_notes_shared::{QuickSwitchItem, SearchHit, SearchResponse, TagCount};
use serde::Deserialize;
use sqlx::Row;

use crate::auth::session::CurrentUser;
use crate::error::AppResult;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

fn clamp_limit(requested: Option<i64>, default: i64, max: i64) -> i64 {
    requested.unwrap_or(default).clamp(1, max)
}

/// Full-text search over note titles and bodies.
///
/// Runs `websearch_to_tsquery`, which understands the syntax people already
/// type into search boxes — quoted phrases, `or`, and a leading `-` to exclude —
/// and, unlike `to_tsquery`, cannot be made to error by unbalanced punctuation.
///
/// When that finds nothing, it falls back to trigram similarity on the title.
/// Full-text search matches whole words after stemming, so it returns nothing
/// for a partial word or a typo, which is exactly when someone searching their
/// own notes most wants a near miss.
pub async fn search(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(params): Query<SearchParams>,
) -> AppResult<Json<SearchResponse>> {
    let query = params.q.trim();
    if query.is_empty() {
        return Ok(Json(SearchResponse { hits: Vec::new() }));
    }
    let limit = clamp_limit(params.limit, 50, 200);

    let rows = sqlx::query(
        "SELECT n.rel_path,
                n.title,
                ts_headline(
                    'english',
                    n.body_text,
                    q,
                    'StartSel=«, StopSel=», MaxWords=28, MinWords=8, \
                     MaxFragments=2, FragmentDelimiter= … '
                ) AS snippet,
                ts_rank(n.search, q) AS rank
         FROM notes n, websearch_to_tsquery('english', $2) AS q
         WHERE n.user_id = $1 AND n.search @@ q
         ORDER BY rank DESC, n.rel_path
         LIMIT $3",
    )
    .bind(user.id)
    .bind(query)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    let mut hits = Vec::with_capacity(rows.len());
    for row in rows {
        hits.push(SearchHit {
            path: row.try_get("rel_path")?,
            title: row.try_get("title")?,
            snippet: row.try_get("snippet")?,
            rank: row.try_get::<f32, _>("rank")?,
        });
    }

    if hits.is_empty() {
        hits = fuzzy_title_search(&state, &user, query, limit).await?;
    }

    Ok(Json(SearchResponse { hits }))
}

/// Trigram fallback: catches partial words and misspellings.
async fn fuzzy_title_search(
    state: &AppState,
    user: &crate::db::User,
    query: &str,
    limit: i64,
) -> AppResult<Vec<SearchHit>> {
    let rows = sqlx::query(
        "SELECT rel_path, title,
                left(body_text, 200) AS snippet,
                similarity(title, $2) AS rank
         FROM notes
         WHERE user_id = $1
           AND (title ILIKE '%' || $2 || '%'
                OR rel_path ILIKE '%' || $2 || '%'
                OR similarity(title, $2) > 0.2)
         ORDER BY rank DESC, length(rel_path), rel_path
         LIMIT $3",
    )
    .bind(user.id)
    .bind(query)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(SearchHit {
                path: row.try_get("rel_path")?,
                title: row.try_get("title")?,
                snippet: row.try_get("snippet")?,
                rank: row.try_get::<f32, _>("rank")?,
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct QuickSwitchParams {
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Titles for the Ctrl+P switcher and for wikilink autocomplete.
///
/// Ordered so that an exact prefix match wins over a substring match, which is
/// what makes typing the first few letters of a note's name land on it rather
/// than on some longer note that happens to contain those letters.
pub async fn quickswitch(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(params): Query<QuickSwitchParams>,
) -> AppResult<Json<Vec<QuickSwitchItem>>> {
    let query = params.q.trim();
    let limit = clamp_limit(params.limit, 20, 100);

    let rows = if query.is_empty() {
        // An empty query opens the switcher on the most recently touched notes,
        // which is almost always where someone wants to go back to.
        sqlx::query(
            "SELECT rel_path, title FROM notes
             WHERE user_id = $1
             ORDER BY mtime DESC
             LIMIT $2",
        )
        .bind(user.id)
        .bind(limit)
        .fetch_all(&state.pool)
        .await?
    } else {
        sqlx::query(
            "SELECT rel_path, title FROM notes
             WHERE user_id = $1
               AND (title ILIKE '%' || $2 || '%' OR rel_path ILIKE '%' || $2 || '%')
             ORDER BY (lower(title) = lower($2)) DESC,
                      (lower(title) LIKE lower($2) || '%') DESC,
                      length(title),
                      title
             LIMIT $3",
        )
        .bind(user.id)
        .bind(query)
        .bind(limit)
        .fetch_all(&state.pool)
        .await?
    };

    let mut items = Vec::with_capacity(rows.len() + 1);
    for row in rows {
        items.push(QuickSwitchItem {
            path: row.try_get("rel_path")?,
            title: row.try_get("title")?,
            exists: true,
        });
    }

    // Offer to create the note when nothing matches exactly. This is what makes
    // a wikilink to a note that does not exist yet a one-keystroke affordance
    // rather than a dead end.
    let has_exact = items
        .iter()
        .any(|item| item.title.eq_ignore_ascii_case(query));
    if !query.is_empty() && !has_exact && go_notes_shared::paths::validate_component(query).is_ok() {
        items.push(QuickSwitchItem {
            path: format!("{query}.md"),
            title: query.to_string(),
            exists: false,
        });
    }

    Ok(Json(items))
}

pub async fn tags(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> AppResult<Json<Vec<TagCount>>> {
    let rows = sqlx::query(
        "SELECT t.name, count(nt.note_id) AS count
         FROM tags t
         JOIN note_tags nt ON nt.tag_id = t.id
         WHERE t.user_id = $1
         GROUP BY t.name
         ORDER BY count DESC, t.name",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(TagCount {
                name: row.try_get("name")?,
                count: row.try_get("count")?,
            })
        })
        .collect::<AppResult<Vec<_>>>()
        .map(Json)
}

#[derive(Debug, Deserialize)]
pub struct TaggedParams {
    pub tag: String,
}

/// Notes carrying a given tag, for clicking through from the tag pane.
pub async fn notes_with_tag(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(params): Query<TaggedParams>,
) -> AppResult<Json<Vec<QuickSwitchItem>>> {
    let rows = sqlx::query(
        "SELECT n.rel_path, n.title
         FROM notes n
         JOIN note_tags nt ON nt.note_id = n.id
         JOIN tags t ON t.id = nt.tag_id
         WHERE n.user_id = $1 AND t.name = $2
         ORDER BY n.title",
    )
    .bind(user.id)
    .bind(params.tag.trim_start_matches('#'))
    .fetch_all(&state.pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(QuickSwitchItem {
                path: row.try_get("rel_path")?,
                title: row.try_get("title")?,
                exists: true,
            })
        })
        .collect::<AppResult<Vec<_>>>()
        .map(Json)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A caller-supplied limit reaches a SQL `LIMIT`, so it must be bounded —
    /// `?limit=100000000` should not become a request to materialise the vault.
    #[test]
    fn limits_are_clamped_to_a_sane_range() {
        assert_eq!(clamp_limit(None, 50, 200), 50);
        assert_eq!(clamp_limit(Some(10), 50, 200), 10);
        assert_eq!(clamp_limit(Some(100_000), 50, 200), 200);
        assert_eq!(clamp_limit(Some(0), 50, 200), 1);
        assert_eq!(clamp_limit(Some(-5), 50, 200), 1);
    }
}
