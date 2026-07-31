//! Server-side sessions.
//!
//! Both sign-in methods — the local password file and Authelia over OIDC —
//! converge here, so the rest of the application never needs to know which one
//! a user came through.
//!
//! Two decisions are worth spelling out.
//!
//! **The cookie holds an opaque random token, not a signed claim.** That means
//! sessions are revocable: deleting the row logs the user out immediately,
//! whereas a self-contained signed token stays valid until it expires no matter
//! what the server wants. It also means there is no signing key to manage,
//! rotate, or leak.
//!
//! **The database stores only a SHA-256 of that token.** A leaked database
//! backup therefore does not hand anyone a working session, in the same way that
//! storing password hashes rather than passwords does. SHA-256 rather than
//! argon2 is deliberate and safe here: unlike a password, the token is 256 bits
//! of uniformly random data, so there is nothing for an attacker to brute-force
//! and no need for a slow hash.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use chrono::Duration;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::db::{self, User};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub const COOKIE_NAME: &str = "go_notes_session";

/// 256 bits, which is beyond any practical guessing attack.
const TOKEN_BYTES: usize = 32;

fn generate_token() -> String {
    let mut buf = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut buf).expect("system random number generator unavailable");
    hex::encode(buf)
}

fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Issues a session and returns the raw token to put in a cookie.
///
/// The raw token is returned exactly once and never stored; after this call it
/// exists only in the user's browser.
pub async fn create(
    pool: &PgPool,
    user_id: Uuid,
    ttl: Duration,
    user_agent: Option<&str>,
) -> AppResult<String> {
    let token = generate_token();

    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at, user_agent)
         VALUES ($1, $2, now() + ($3::bigint * interval '1 second'), $4)",
    )
    .bind(hash_token(&token))
    .bind(user_id)
    .bind(ttl.num_seconds())
    .bind(user_agent.map(|ua| ua.chars().take(400).collect::<String>()))
    .execute(pool)
    .await?;

    Ok(token)
}

/// Looks up the user behind a token, sliding the expiry forward as it goes.
///
/// The renewal is what makes "remember me" behave the way people expect: an
/// active user is never logged out mid-session, while an abandoned session still
/// expires a fixed time after it was last used. Expiry is only extended once it
/// is more than halfway gone, so a busy tab does not issue a write per request.
pub async fn resolve(pool: &PgPool, token: &str, ttl: Duration) -> AppResult<Option<User>> {
    let half_life = ttl.num_seconds() / 2;

    let row = sqlx::query(
        "WITH touched AS (
             UPDATE sessions
             SET last_seen_at = now(),
                 expires_at = CASE
                     WHEN expires_at < now() + ($3::bigint * interval '1 second')
                     THEN now() + ($2::bigint * interval '1 second')
                     ELSE expires_at
                 END
             WHERE token_hash = $1 AND expires_at > now()
             RETURNING user_id
         )
         SELECT u.id, u.username::text AS username, u.display_name, u.email,
                u.auth_provider, u.auth_subject, u.vault_dir
         FROM users u JOIN touched ON touched.user_id = u.id",
    )
    .bind(hash_token(token))
    .bind(ttl.num_seconds())
    .bind(half_life)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(User {
        id: row.try_get("id")?,
        username: row.try_get("username")?,
        display_name: row.try_get("display_name")?,
        email: row.try_get("email")?,
        auth_provider: row.try_get("auth_provider")?,
        auth_subject: row.try_get("auth_subject")?,
        vault_dir: row.try_get("vault_dir")?,
    }))
}

pub async fn destroy(pool: &PgPool, token: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
        .bind(hash_token(token))
        .execute(pool)
        .await?;
    Ok(())
}

/// Signs a user out everywhere. Used by the CLI when a password is changed —
/// a password reset that leaves old sessions alive is not really a reset.
pub async fn destroy_all_for_user(pool: &PgPool, user_id: Uuid) -> AppResult<u64> {
    let result = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(user_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Clears out expired sessions and abandoned login attempts.
pub async fn purge_expired(pool: &PgPool) -> AppResult<u64> {
    let sessions = sqlx::query("DELETE FROM sessions WHERE expires_at < now()")
        .execute(pool)
        .await?
        .rows_affected();

    sqlx::query("DELETE FROM login_flows WHERE expires_at < now()")
        .execute(pool)
        .await?;

    Ok(sessions)
}

/// Builds the `Set-Cookie` that carries a session.
pub fn build_cookie(token: String, ttl: Duration, secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, token);
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    // `Lax` rather than `Strict` so that following a link into the app from
    // elsewhere — including the redirect back from Authelia — arrives logged in.
    // Combined with the Origin check on every mutating request, this is not a
    // CSRF weakness.
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::seconds(ttl.num_seconds()));
    cookie
}

/// The matching cookie that clears a session.
pub fn clearing_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(COOKIE_NAME, "");
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::seconds(0));
    cookie
}

pub fn token_from_jar(jar: &CookieJar) -> Option<String> {
    jar.get(COOKIE_NAME)
        .map(|cookie| cookie.value().to_string())
        .filter(|token| !token.is_empty())
}

/// Extractor for handlers that require a signed-in user.
///
/// Every route that touches a vault takes this, so "did we check who this is?"
/// is answered by the function signature rather than by remembering to call
/// something at the top of the body.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub User);

impl std::ops::Deref for CurrentUser {
    type Target = User;
    fn deref(&self) -> &User {
        &self.0
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = token_from_jar(&jar).ok_or(AppError::Unauthenticated)?;

        let user = resolve(&state.pool, &token, state.config.session_ttl())
            .await?
            .ok_or(AppError::Unauthenticated)?;

        Ok(CurrentUser(user))
    }
}

/// Extractor for routes that behave differently when signed in but do not
/// require it, such as the page shell itself.
pub struct MaybeUser(pub Option<User>);

impl FromRequestParts<AppState> for MaybeUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let Some(token) = token_from_jar(&jar) else {
            return Ok(MaybeUser(None));
        };
        let user = resolve(&state.pool, &token, state.config.session_ttl())
            .await
            .ok()
            .flatten();
        Ok(MaybeUser(user))
    }
}

/// Signs a user in: records the login and returns the cookie to set.
pub async fn establish(
    state: &AppState,
    user: &User,
    user_agent: Option<&str>,
) -> AppResult<Cookie<'static>> {
    let ttl = state.config.session_ttl();
    let token = create(&state.pool, user.id, ttl, user_agent).await?;
    db::touch_login(&state.pool, user.id).await?;

    tracing::info!(
        user = %user.username,
        provider = %user.auth_provider,
        "signed in"
    );
    Ok(build_cookie(token, ttl, state.config.auth.cookie_secure))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_unpredictable() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// What is stored must not be what is presented, or a database dump would
    /// be a list of working session cookies.
    #[test]
    fn stored_hash_differs_from_the_token() {
        let token = generate_token();
        let hash = hash_token(&token);
        assert_eq!(hash.len(), 32);
        assert_ne!(hex::encode(&hash), token);
        // And it must be deterministic, or lookups would never match.
        assert_eq!(hash, hash_token(&token));
    }

    #[test]
    fn session_cookie_carries_the_protective_attributes() {
        let cookie = build_cookie("abc".into(), Duration::days(30), true);
        assert!(cookie.http_only().unwrap());
        assert!(cookie.secure().unwrap());
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert_eq!(cookie.path(), Some("/"));
    }

    #[test]
    fn clearing_cookie_expires_immediately() {
        let cookie = clearing_cookie(true);
        assert_eq!(cookie.value(), "");
        assert_eq!(cookie.max_age(), Some(time::Duration::seconds(0)));
    }

    /// Plain-HTTP development must not silently ship `Secure` cookies, which the
    /// browser would then refuse to send at all.
    #[test]
    fn secure_attribute_follows_configuration() {
        assert!(!build_cookie("abc".into(), Duration::days(1), false)
            .secure()
            .unwrap());
    }
}
