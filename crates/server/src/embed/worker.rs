//! The background pass: embed what has no vector, then recompute the edges.
//!
//! Modelled on `spawn_session_cleanup` in `main.rs`, which is the only other
//! recurring timer in this codebase: a plain `tokio::time::interval`, errors
//! logged, the loop continues. There is no job table and no retry framework,
//! because the work is idempotent and self-describing — "which passages have no
//! vector" is a query, not a queue, so a crash mid-pass costs one batch and the
//! next tick picks up exactly where it left off.

use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::config::EmbeddingsConfig;
use crate::embed::similarity::{self, Passage};
use crate::embed::EmbeddingClient;

/// What one pass did, for the CLI to print and the log to report.
#[derive(Debug, Default, Clone, Copy)]
pub struct EmbedReport {
    pub embedded: usize,
    pub notes_relinked: usize,
}

/// Runs the pass forever, on an interval.
///
/// Spawned and then let go, like the startup reconcile: nothing waits on it, and
/// dropping it at shutdown costs at most one in-flight batch that the next start
/// will redo.
pub fn spawn(pool: PgPool, client: EmbeddingClient, config: EmbeddingsConfig) {
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_secs(config.interval_secs.max(5)));
        let mut state = PassState::default();
        // A wrong URL should not be retried at full speed forever; this backs
        // off to a few minutes and recovers the moment a pass succeeds.
        let mut failures: u32 = 0;

        loop {
            ticker.tick().await;
            if failures > 0 {
                let wait = std::cmp::min(1 << failures.min(5), 32) * config.interval_secs.max(5);
                tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
            }

            match run_once(&pool, &client, &config, &mut state).await {
                Ok(report) => {
                    failures = 0;
                    if report.embedded > 0 {
                        tracing::info!(
                            embedded = report.embedded,
                            notes = report.notes_relinked,
                            "embedded new passages"
                        );
                    }
                }
                Err(err) => {
                    failures = failures.saturating_add(1);
                    tracing::warn!(error = ?err, failures, "embedding pass failed");
                }
            }
        }
    });
}

/// What the last relink for a user was computed from.
///
/// Relinking only when something was *newly embedded* is not enough, and the way
/// it fails is quiet: a passage moved between two notes produces text that is
/// already in the cache, so nothing is embedded and the edges keep pointing at
/// the note that used to hold it. A restart with a full cache would likewise
/// never rebuild edges that had been truncated.
///
/// So the decision is made on the shape of the input instead — how many passages
/// there are, the high-water mark of their ids, and how many have vectors. Any
/// edit that adds, removes or changes a passage moves at least one of those.
/// Cheap enough to ask every tick, and an empty map means "relink", which is what
/// makes the CLI and a fresh process both do the right thing.
#[derive(Default)]
pub struct PassState {
    seen: std::collections::HashMap<Uuid, (i64, i64, i64)>,
}

/// One pass over every user: embed what is missing, then relink what changed.
pub async fn run_once(
    pool: &PgPool,
    client: &EmbeddingClient,
    config: &EmbeddingsConfig,
    state: &mut PassState,
) -> Result<EmbedReport> {
    let mut report = EmbedReport::default();
    let users: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM users")
        .fetch_all(pool)
        .await
        .context("listing users")?;

    // Held rather than returned, so that an unreachable model does not also stop
    // the edges being rebuilt from the vectors already stored. Letting `?` out of
    // the loop meant one passage nobody could embed — a note written while the
    // model was down, or the first pass after `TRUNCATE ... CASCADE` — kept
    // `semantic_links` empty even though every other passage had a good vector.
    // The graph lost every suggestion it already had, until the endpoint came
    // back. Reported at the end instead, so the worker still backs off.
    let mut failure: Option<anyhow::Error> = None;

    for user_id in users {
        match embed_missing(pool, client, config, user_id).await {
            Ok(count) => report.embedded += count,
            Err(err) => {
                tracing::warn!(
                    error = ?err,
                    %user_id,
                    "could not embed new passages; relinking with the vectors already stored"
                );
                failure.get_or_insert(err);
            }
        }

        let fingerprint = fingerprint(pool, client, user_id).await?;
        if state.seen.get(&user_id) == Some(&fingerprint) {
            continue;
        }
        report.notes_relinked += relink_user(pool, client, config, user_id).await?;
        state.seen.insert(user_id, fingerprint);
    }

    match failure {
        Some(err) => Err(err),
        None => Ok(report),
    }
}

/// `(passages, highest passage id, passages with a vector)` for one user.
async fn fingerprint(
    pool: &PgPool,
    client: &EmbeddingClient,
    user_id: Uuid,
) -> Result<(i64, i64, i64)> {
    let row = sqlx::query(
        "SELECT count(*) AS chunks,
                COALESCE(max(c.id), 0) AS high,
                count(e.body_hash) AS embedded
         FROM note_chunks c
         LEFT JOIN embeddings e
           ON e.user_id = c.user_id AND e.body_hash = c.body_hash AND e.model = $2
         WHERE c.user_id = $1",
    )
    .bind(user_id)
    .bind(client.model())
    .fetch_one(pool)
    .await
    .context("fingerprinting a user's passages")?;

    Ok((
        row.try_get("chunks")?,
        row.try_get("high")?,
        row.try_get("embedded")?,
    ))
}

/// Embeds every passage of one user that has no vector for the current model.
///
/// Batched, and the batch is deduplicated by hash first: a phrase repeated
/// across a vault is one call, not twenty.
async fn embed_missing(
    pool: &PgPool,
    client: &EmbeddingClient,
    config: &EmbeddingsConfig,
    user_id: Uuid,
) -> Result<usize> {
    let mut total = 0;

    loop {
        // `DISTINCT` on the hash, because the same passage in two notes needs
        // one vector; `NOT EXISTS` rather than a LEFT JOIN so the index on
        // `embeddings` can answer it directly.
        let rows = sqlx::query(
            "SELECT DISTINCT c.body_hash, c.heading, c.body
             FROM note_chunks c
             WHERE c.user_id = $1
               AND NOT EXISTS (
                   SELECT 1 FROM embeddings e
                   WHERE e.user_id = c.user_id AND e.model = $2 AND e.body_hash = c.body_hash
               )
             LIMIT $3",
        )
        .bind(user_id)
        .bind(client.model())
        .bind(config.batch_size as i64)
        .fetch_all(pool)
        .await
        .context("finding passages with no embedding")?;

        if rows.is_empty() {
            return Ok(total);
        }

        let mut hashes = Vec::with_capacity(rows.len());
        let mut inputs = Vec::with_capacity(rows.len());
        for row in &rows {
            let heading: String = row.try_get("heading")?;
            let body: String = row.try_get("body")?;
            hashes.push(row.try_get::<String, _>("body_hash")?);
            inputs.push(if heading.is_empty() {
                body
            } else {
                format!("{heading}\n\n{body}")
            });
        }

        let vectors = client.embed(&inputs).await?;
        let count = vectors.len();

        for (hash, mut vector) in hashes.into_iter().zip(vectors) {
            // Normalised on the way in, once, so every comparison afterwards is
            // a dot product and no query has to remember to do this.
            similarity::normalise(&mut vector);
            sqlx::query(
                "INSERT INTO embeddings (user_id, model, body_hash, dims, vector)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (user_id, model, body_hash) DO NOTHING",
            )
            .bind(user_id)
            .bind(client.model())
            .bind(&hash)
            .bind(vector.len() as i32)
            .bind(similarity::pack(&vector))
            .execute(pool)
            .await
            .context("storing an embedding")?;
        }

        total += count;

        // A partial batch means that was everything there was to do.
        if count < config.batch_size {
            return Ok(total);
        }
    }
}

/// Recomputes every semantic edge for one user.
///
/// Brute force: each note's passages are scored against every embedded passage
/// in the vault. That is honest about what it costs — it is linear in the vault
/// per note and quadratic overall — and at the sizes this is built for it is a
/// few seconds. Past roughly a hundred thousand passages it wants an index
/// instead, and the README says so rather than leaving it to be discovered.
async fn relink_user(
    pool: &PgPool,
    client: &EmbeddingClient,
    config: &EmbeddingsConfig,
    user_id: Uuid,
) -> Result<usize> {
    let corpus = load_passages(pool, client, user_id).await?;
    if corpus.is_empty() {
        return Ok(0);
    }

    let mut by_note: std::collections::HashMap<Uuid, Vec<usize>> = std::collections::HashMap::new();
    for (index, passage) in corpus.iter().enumerate() {
        by_note.entry(passage.note_id).or_default().push(index);
    }

    let neighbours = config.neighbours;
    let min_score = config.min_score;

    // The comparisons are pure CPU over a few megabytes, and there can be
    // millions of them; on the async runtime they would stall every request
    // sharing the thread. `store.rs` offloads its directory walks for the same
    // reason.
    let edges = tokio::task::spawn_blocking(move || {
        let mut edges: Vec<(Uuid, similarity::Neighbour)> = Vec::new();
        for (note_id, indices) in &by_note {
            let mine: Vec<Passage> = indices
                .iter()
                .map(|i| Passage {
                    note_id: corpus[*i].note_id,
                    ordinal: corpus[*i].ordinal,
                    vector: corpus[*i].vector.clone(),
                })
                .collect();
            for found in
                similarity::best_neighbours(&mine, &corpus, *note_id, neighbours, min_score)
            {
                edges.push((*note_id, found));
            }
        }

        // Every note's neighbours are found independently, so a mutual match
        // between A and B is discovered twice — once from each side — and
        // without this, both directed rows would be inserted under the primary
        // key `(source_note_id, target_note_id)`, which does not consider them
        // duplicates. `suggested_for` unions "I'm the source" with "I'm the
        // target" on the assumption that a pair is one row; two rows made every
        // suggestion appear twice. Canonicalising on note id, rather than
        // keeping whichever direction happened to be found first, keeps the
        // stored row the same across rebuilds regardless of `by_note`'s
        // (randomised) hash iteration order.
        let mut canonical: std::collections::HashMap<(Uuid, Uuid), (Uuid, similarity::Neighbour)> =
            std::collections::HashMap::new();
        for (source, edge) in edges {
            let (source, edge) = if source <= edge.target_note_id {
                (source, edge)
            } else {
                (
                    edge.target_note_id,
                    similarity::Neighbour {
                        target_note_id: source,
                        score: edge.score,
                        source_ordinal: edge.target_ordinal,
                        target_ordinal: edge.source_ordinal,
                    },
                )
            };
            let key = (source, edge.target_note_id);
            let keep = match canonical.get(&key) {
                Some((_, existing)) => edge.score > existing.score,
                None => true,
            };
            if keep {
                canonical.insert(key, (source, edge));
            }
        }
        canonical.into_values().collect::<Vec<_>>()
    })
    .await
    .context("scoring passages")?;

    // Replaced wholesale rather than merged: an edge that should no longer exist
    // has no row saying so, and leaving stale ones would make the graph slowly
    // fill with relationships to notes that have since been rewritten.
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM semantic_links WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;

    let touched = edges.len();
    for (source, edge) in edges {
        sqlx::query(
            "INSERT INTO semantic_links
                 (user_id, source_note_id, target_note_id, score, source_ordinal, target_ordinal)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (source_note_id, target_note_id) DO UPDATE
             SET score = EXCLUDED.score,
                 source_ordinal = EXCLUDED.source_ordinal,
                 target_ordinal = EXCLUDED.target_ordinal,
                 computed_at = now()",
        )
        .bind(user_id)
        .bind(source)
        .bind(edge.target_note_id)
        .bind(edge.score)
        .bind(edge.source_ordinal)
        .bind(edge.target_ordinal)
        .execute(&mut *tx)
        .await
        .context("storing a semantic link")?;
    }
    tx.commit().await?;

    Ok(touched)
}

async fn load_passages(
    pool: &PgPool,
    client: &EmbeddingClient,
    user_id: Uuid,
) -> Result<Vec<Passage>> {
    let rows = sqlx::query(
        "SELECT c.note_id, c.ordinal, e.vector
         FROM note_chunks c
         JOIN embeddings e
           ON e.user_id = c.user_id AND e.body_hash = c.body_hash AND e.model = $2
         WHERE c.user_id = $1
         ORDER BY c.note_id, c.ordinal",
    )
    .bind(user_id)
    .bind(client.model())
    .fetch_all(pool)
    .await
    .context("loading passages")?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let vector = similarity::unpack(row.try_get::<Vec<u8>, _>("vector")?.as_slice());
        if vector.is_empty() {
            continue;
        }
        out.push(Passage {
            note_id: row.try_get("note_id")?,
            ordinal: row.try_get("ordinal")?,
            vector,
        });
    }
    Ok(out)
}
