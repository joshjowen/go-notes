//! Database connection, migrations, and the `users` table.
//!
//! Note on query style: this crate uses sqlx's runtime `query_as` rather than
//! the compile-time-checked `query!` macros. The macros need either a live
//! database at build time or a committed `.sqlx` cache that must be regenerated
//! on every query edit; neither is worth imposing on a `podman build` of this
//! image. The queries are exercised against a real Postgres instead, by
//! `crates/server/tests/integration.rs` — run with `--features integration`.

use std::time::Duration;

use anyhow::{Context, Result};
use go_notes_shared::paths;
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// A row of `users`, plus the vault directory that row owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub auth_provider: String,
    pub auth_subject: String,
    pub vault_dir: String,
}

impl User {
    fn from_row(row: &PgRow) -> sqlx::Result<User> {
        Ok(User {
            id: row.try_get("id")?,
            username: row.try_get("username")?,
            display_name: row.try_get("display_name")?,
            email: row.try_get("email")?,
            auth_provider: row.try_get("auth_provider")?,
            auth_subject: row.try_get("auth_subject")?,
            vault_dir: row.try_get("vault_dir")?,
        })
    }

    pub fn to_me(&self) -> go_notes_shared::Me {
        go_notes_shared::Me {
            username: self.username.clone(),
            display_name: self.display_name.clone(),
            email: self.email.clone(),
            auth_provider: self.auth_provider.clone(),
        }
    }
}

// Every select spells out its columns rather than interpolating a shared
// constant. sqlx 0.9 refuses runtime-built query strings outright, and writing
// them literally means the column list is visible at each call site.
//
// `username` is `citext`, which sqlx has no native mapping for, so it is always
// cast back to `text`.

/// Connects to Postgres, retrying until `connect_timeout_secs` elapses.
///
/// The retry loop is not optional in practice. Both `docker compose` and
/// systemd start this container as soon as the Postgres container exists, which
/// is well before Postgres is accepting queries — and podman-compose honours
/// `depends_on` more loosely than Docker does, so waiting here rather than
/// relying on the orchestrator is what makes both work the same way.
pub async fn connect(config: &crate::config::DatabaseConfig) -> Result<PgPool> {
    let deadline = std::time::Instant::now() + Duration::from_secs(config.connect_timeout_secs);
    let mut delay = Duration::from_millis(250);
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        let result = PgPoolOptions::new()
            .max_connections(config.max_connections)
            .acquire_timeout(Duration::from_secs(10))
            .connect(&config.url)
            .await;

        match result {
            Ok(pool) => {
                if attempt > 1 {
                    tracing::info!(attempt, "connected to postgres");
                }
                return Ok(pool);
            }
            Err(err) if std::time::Instant::now() < deadline => {
                tracing::warn!(
                    attempt,
                    error = %err,
                    retry_in_ms = delay.as_millis() as u64,
                    "postgres is not ready yet"
                );
                tokio::time::sleep(delay).await;
                // Back off, but keep checking often enough that startup is not
                // needlessly slow once the database does come up.
                delay = (delay * 2).min(Duration::from_secs(5));
            }
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "could not connect to postgres within {}s",
                    config.connect_timeout_secs
                )))
            }
        }
    }
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .context("running database migrations")?;
    Ok(())
}

pub async fn find_user_by_id(pool: &PgPool, id: Uuid) -> sqlx::Result<Option<User>> {
    let row = sqlx::query(
        "SELECT id, username::text AS username, display_name, email,
                auth_provider, auth_subject, vault_dir
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(User::from_row).transpose()
}

pub async fn find_user_by_subject(
    pool: &PgPool,
    provider: &str,
    subject: &str,
) -> sqlx::Result<Option<User>> {
    let row = sqlx::query(
        "SELECT id, username::text AS username, display_name, email,
                auth_provider, auth_subject, vault_dir
         FROM users WHERE auth_provider = $1 AND auth_subject = $2",
    )
    .bind(provider)
    .bind(subject)
    .fetch_optional(pool)
    .await?;
    row.as_ref().map(User::from_row).transpose()
}

pub async fn list_users(pool: &PgPool) -> sqlx::Result<Vec<User>> {
    let rows = sqlx::query(
        "SELECT id, username::text AS username, display_name, email,
                auth_provider, auth_subject, vault_dir
         FROM users ORDER BY username",
    )
    .fetch_all(pool)
    .await?;
    rows.iter().map(User::from_row).collect()
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub display_name: &'a str,
    pub email: Option<&'a str>,
    pub auth_provider: &'a str,
    pub auth_subject: &'a str,
}

/// Finds the user behind an identity, creating them on first sign-in.
///
/// Identity is keyed on `(auth_provider, auth_subject)`, never on the username.
/// For OIDC the subject is the provider's `sub`, which the spec requires to be
/// stable and never reassigned — so a user who renames themselves upstream
/// keeps their vault, and a *new* user who happens to claim the old username
/// gets their own rather than inheriting someone else's notes.
pub async fn find_or_create_user(pool: &PgPool, new: NewUser<'_>) -> Result<User> {
    if let Some(user) = find_user_by_subject(pool, new.auth_provider, new.auth_subject).await? {
        // Display name and email are allowed to drift; the vault never moves.
        if user.display_name != new.display_name || user.email.as_deref() != new.email {
            sqlx::query("UPDATE users SET display_name = $2, email = $3 WHERE id = $1")
                .bind(user.id)
                .bind(new.display_name)
                .bind(new.email)
                .execute(pool)
                .await?;
        }
        return Ok(user);
    }

    // The vault directory is derived from the username but is not required to
    // equal it: usernames can contain characters that are not safe filenames,
    // and two different usernames can sanitise to the same string.
    let base = paths::sanitize_component(new.username, "user");

    for suffix in 0..64u32 {
        let vault_dir = if suffix == 0 {
            base.clone()
        } else {
            format!("{base}-{suffix}")
        };

        let result = sqlx::query(
            "INSERT INTO users (username, display_name, email,
                                auth_provider, auth_subject, vault_dir)
             VALUES ($1, $2, $3, $4, $5, $6)
             RETURNING id, username::text AS username, display_name, email,
                       auth_provider, auth_subject, vault_dir",
        )
        .bind(new.username)
        .bind(new.display_name)
        .bind(new.email)
        .bind(new.auth_provider)
        .bind(new.auth_subject)
        .bind(&vault_dir)
        .fetch_one(pool)
        .await;

        match result {
            Ok(row) => return Ok(User::from_row(&row)?),
            Err(err) if is_unique_violation(&err, "users_vault_dir_key") => {
                // Someone else already owns that directory name; try the next.
                continue;
            }
            Err(err) if is_unique_violation(&err, "users_username_key") => {
                // A concurrent request created this user between our lookup and
                // our insert. Their row is as good as ours.
                if let Some(user) =
                    find_user_by_subject(pool, new.auth_provider, new.auth_subject).await?
                {
                    return Ok(user);
                }
                anyhow::bail!("username '{}' is already taken", new.username);
            }
            Err(err) => return Err(anyhow::Error::new(err).context("creating user")),
        }
    }

    anyhow::bail!("could not find a free vault directory name for '{}'", new.username)
}

pub async fn touch_login(pool: &PgPool, id: Uuid) -> sqlx::Result<()> {
    sqlx::query("UPDATE users SET last_login_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

fn is_unique_violation(err: &sqlx::Error, constraint: &str) -> bool {
    let sqlx::Error::Database(db_err) = err else {
        return false;
    };
    // 23505 is the SQLSTATE for unique_violation.
    db_err.code().as_deref() == Some("23505")
        && db_err.constraint().is_some_and(|c| c == constraint)
}
