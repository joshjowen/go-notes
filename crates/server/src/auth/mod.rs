//! Authentication: who is making this request.
//!
//! Two sign-in methods are supported and both can be enabled at once, which is
//! genuinely useful — an operator can run Authelia for everyday use and keep one
//! local account as a way back in when the identity provider is down.
//!
//! Whichever route a user takes, they end up with the same server-side session
//! ([`session`]) and the same `users` row, so nothing downstream branches on how
//! they signed in.

pub mod local;
pub mod oidc;
pub mod session;
pub mod throttle;

use anyhow::Result;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::db::{self, NewUser, User};
use crate::error::AppResult;

pub const PROVIDER_LOCAL: &str = "local";
pub const PROVIDER_OIDC: &str = "oidc";

/// How long a half-finished OIDC sign-in stays valid.
///
/// Long enough for someone to actually type a password and complete a second
/// factor, short enough that abandoned attempts do not accumulate.
const FLOW_TTL_SECONDS: i64 = 10 * 60;

/// An in-flight OIDC authorization attempt, retrieved on the callback.
#[derive(Debug, Clone)]
pub struct StoredFlow {
    pub csrf_state: String,
    pub nonce: String,
    pub pkce_verifier: String,
    pub redirect_to: Option<String>,
}

/// Parks the values that must survive the round trip to the provider.
///
/// These live in Postgres rather than in a cookie so the PKCE verifier and nonce
/// never reach the browser at all. The cookie carries only the opaque row id, so
/// there is nothing in it worth stealing and nothing to sign.
pub async fn store_flow(
    pool: &PgPool,
    flow: &oidc::PendingFlow,
    redirect_to: Option<&str>,
) -> AppResult<Uuid> {
    let id: Uuid = sqlx::query(
        "INSERT INTO login_flows (csrf_state, nonce, pkce_verifier, redirect_to, expires_at)
         VALUES ($1, $2, $3, $4, now() + ($5::bigint * interval '1 second'))
         RETURNING id",
    )
    .bind(&flow.csrf_state)
    .bind(&flow.nonce)
    .bind(&flow.pkce_verifier)
    .bind(redirect_to)
    .bind(FLOW_TTL_SECONDS)
    .fetch_one(pool)
    .await?
    .try_get("id")?;
    Ok(id)
}

/// Retrieves and consumes a stored flow.
///
/// The delete is part of the same statement as the read, which is what makes an
/// authorization code single-use: a replayed callback finds nothing and fails,
/// even if two requests arrive simultaneously.
pub async fn take_flow(pool: &PgPool, id: Uuid) -> AppResult<Option<StoredFlow>> {
    let row = sqlx::query(
        "DELETE FROM login_flows
         WHERE id = $1 AND expires_at > now()
         RETURNING csrf_state, nonce, pkce_verifier, redirect_to",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(StoredFlow {
        csrf_state: row.try_get("csrf_state")?,
        nonce: row.try_get("nonce")?,
        pkce_verifier: row.try_get("pkce_verifier")?,
        redirect_to: row.try_get("redirect_to")?,
    }))
}

/// Maps a verified OIDC identity onto a `users` row, creating it on first login.
pub async fn provision_oidc_user(pool: &PgPool, identity: &oidc::OidcIdentity) -> Result<User> {
    db::find_or_create_user(
        pool,
        NewUser {
            username: &identity.username,
            display_name: &identity.display_name,
            email: identity.email.as_deref(),
            auth_provider: PROVIDER_OIDC,
            auth_subject: &identity.subject,
        },
    )
    .await
}

/// Maps an entry from the local users file onto a `users` row.
///
/// The subject is the username, lowercased. Unlike an OIDC `sub` this is not
/// immutable — renaming someone in `users.json` creates a new account rather
/// than renaming the existing one — which is documented in the README, along
/// with the `go-notes user rename` subcommand that does it properly.
pub async fn provision_local_user(pool: &PgPool, user: &local::LocalUser) -> Result<User> {
    db::find_or_create_user(
        pool,
        NewUser {
            username: &user.username,
            display_name: user.display_name(),
            email: user.email.as_deref(),
            auth_provider: PROVIDER_LOCAL,
            auth_subject: &user.username.to_lowercase(),
        },
    )
    .await
}

/// Validates a post-login redirect supplied by the client.
///
/// Only same-site paths are allowed. Without this check, `/api/auth/oidc/login?
/// redirect_to=https://evil.example` would turn the app into an open redirector,
/// which is a standard way to make a phishing link look legitimate.
pub fn safe_redirect(candidate: Option<&str>) -> Option<String> {
    let candidate = candidate?;

    // Must be a rooted path, and must not be protocol-relative (`//evil.com`),
    // which browsers resolve as an absolute URL to another host.
    if !candidate.starts_with('/') || candidate.starts_with("//") {
        return None;
    }
    // A backslash is treated as a separator by some browsers, so `/\evil.com`
    // can escape as well.
    if candidate.contains('\\') || candidate.contains('\r') || candidate.contains('\n') {
        return None;
    }
    Some(candidate.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_same_site_paths() {
        for candidate in ["/", "/note/Projects/A.md", "/?tab=graph", "/a#anchor"] {
            assert_eq!(
                safe_redirect(Some(candidate)),
                Some(candidate.to_string()),
                "{candidate} should be accepted"
            );
        }
    }

    /// Everything here is a way of smuggling an absolute URL past a naive
    /// "starts with a slash" check.
    #[test]
    fn refuses_anything_that_could_leave_the_site() {
        let hostile = [
            "https://evil.example",
            "//evil.example",
            "/\\evil.example",
            "\\\\evil.example",
            "javascript:alert(1)",
            "/path\r\nSet-Cookie: a=b",
            "/path\nLocation: https://evil.example",
            "relative/path",
            "",
        ];
        for candidate in hostile {
            assert_eq!(
                safe_redirect(Some(candidate)),
                None,
                "{candidate:?} should be refused"
            );
        }
        assert_eq!(safe_redirect(None), None);
    }
}
