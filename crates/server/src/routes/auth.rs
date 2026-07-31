//! Sign-in, sign-out, and the OIDC round trip.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Json;
use axum_extra::extract::cookie::{Cookie, CookieJar, SameSite};
use go_notes_shared::{AuthInfo, LoginRequest, Me};
use serde::Deserialize;
use uuid::Uuid;

use crate::auth::session::{self, CurrentUser, MaybeUser};
use crate::auth::{self, PROVIDER_OIDC};
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::web;

/// Cookie holding the id of an in-flight OIDC attempt.
const FLOW_COOKIE: &str = "go_notes_oidc_flow";

/// What sign-in methods this deployment offers. Fetched before login, so it is
/// the one endpoint that must work unauthenticated.
pub async fn info(State(state): State<AppState>) -> Json<AuthInfo> {
    Json(state.auth_info())
}

pub async fn me(CurrentUser(user): CurrentUser) -> Json<Me> {
    Json(user.to_me())
}

/// Username and password against the local file.
pub async fn login(
    State(state): State<AppState>,
    jar: CookieJar,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> AppResult<Response> {
    let Some(local_users) = state.local_users.clone() else {
        return Err(AppError::forbidden(
            "password sign-in is not enabled on this server",
        ));
    };

    let client = web::client_address(&state, &headers, Some(peer));
    let throttle_key = crate::auth::throttle::LoginThrottle::key(&client, &body.username);

    if let Some(wait) = state.throttle.check(&throttle_key) {
        tracing::warn!(
            client = %client,
            username = %body.username,
            "rejecting login attempt: throttled"
        );
        return Ok((
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", wait.as_secs().max(1).to_string())],
            Json(go_notes_shared::ApiError {
                code: "throttled".into(),
                message: format!(
                    "Too many failed attempts. Try again in {} seconds.",
                    wait.as_secs().max(1)
                ),
            }),
        )
            .into_response());
    }

    let Some(local_user) = local_users.verify(&body.username, &body.password) else {
        state.throttle.record_failure(&throttle_key);
        tracing::warn!(client = %client, username = %body.username, "failed password login");

        // Deliberately identical whether the username exists or not, so the
        // response body reveals no more than the response timing does.
        return Err(AppError::Unauthenticated);
    };

    state.throttle.record_success(&throttle_key);

    let user = auth::provision_local_user(&state.pool, &local_user).await?;
    let cookie = session::establish(&state, &user, web::user_agent(&headers).as_deref()).await?;

    Ok((jar.add(cookie), Json(user.to_me())).into_response())
}

#[derive(Debug, serde::Serialize)]
pub struct LogoutResponse {
    /// Where the client should navigate next. Set when the identity provider
    /// supports RP-initiated logout, so signing out here also signs the user out
    /// of Authelia rather than leaving them able to walk straight back in.
    pub redirect_to: Option<String>,
}

pub async fn logout(
    State(state): State<AppState>,
    jar: CookieJar,
    // `MaybeUser` rather than `CurrentUser`: signing out must work even when the
    // session has already expired, otherwise the cookie can never be cleared.
    MaybeUser(user): MaybeUser,
) -> AppResult<Response> {
    if let Some(token) = session::token_from_jar(&jar) {
        session::destroy(&state.pool, &token).await?;
    }

    let redirect_to = match (&state.oidc, user) {
        (Some(provider), Some(user))
            if user.auth_provider == PROVIDER_OIDC && state.config.auth.oidc.end_session =>
        {
            provider.end_session_url(&state.config.server.public_url)
        }
        _ => None,
    };

    let jar = jar.add(session::clearing_cookie(state.config.auth.cookie_secure));
    Ok((jar, Json(LogoutResponse { redirect_to })).into_response())
}

#[derive(Debug, Deserialize)]
pub struct OidcLoginParams {
    /// Where to land after signing in, so a deep link survives the round trip.
    pub redirect_to: Option<String>,
}

/// Step one of the OIDC flow: send the browser to the provider.
pub async fn oidc_login(
    State(state): State<AppState>,
    jar: CookieJar,
    Query(params): Query<OidcLoginParams>,
) -> AppResult<Response> {
    let Some(provider) = state.oidc.clone() else {
        return Err(AppError::forbidden(
            "single sign-on is not enabled on this server",
        ));
    };

    let flow = provider.begin();
    // `safe_redirect` is what stops `?redirect_to=https://evil.example` turning
    // this endpoint into an open redirector.
    let redirect_to = auth::safe_redirect(params.redirect_to.as_deref());
    let flow_id = auth::store_flow(&state.pool, &flow, redirect_to.as_deref()).await?;

    let jar = jar.add(flow_cookie(
        flow_id.to_string(),
        state.config.auth.cookie_secure,
    ));
    Ok((jar, Redirect::to(&flow.authorize_url)).into_response())
}

#[derive(Debug, Deserialize)]
pub struct OidcCallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// Step two: the provider sends the browser back with an authorization code.
pub async fn oidc_callback(
    State(state): State<AppState>,
    jar: CookieJar,
    headers: HeaderMap,
    Query(params): Query<OidcCallbackParams>,
) -> AppResult<Response> {
    let Some(provider) = state.oidc.clone() else {
        return Err(AppError::forbidden("single sign-on is not enabled"));
    };

    let cleared = jar.clone().add(clearing_flow_cookie(
        state.config.auth.cookie_secure,
    ));

    // The provider can report a failure instead of a code — a cancelled login,
    // or an access-control rule in Authelia that denied this user.
    if let Some(error) = &params.error {
        tracing::warn!(
            error = %error,
            description = params.error_description.as_deref().unwrap_or(""),
            "identity provider refused the sign-in"
        );
        return Err(AppError::forbidden(
            "the identity provider refused this sign-in",
        ));
    }

    let flow_id = jar
        .get(FLOW_COOKIE)
        .and_then(|cookie| Uuid::parse_str(cookie.value()).ok())
        .ok_or_else(|| {
            AppError::bad_request("this sign-in link has expired; please try again")
        })?;

    // Consuming the flow is what makes the callback single-use: a replayed
    // request finds nothing here and stops.
    let flow = auth::take_flow(&state.pool, flow_id).await?.ok_or_else(|| {
        AppError::bad_request("this sign-in attempt has expired; please try again")
    })?;

    let returned_state = params
        .state
        .as_deref()
        .ok_or_else(|| AppError::bad_request("the identity provider returned no state"))?;

    // Compared in constant time. The consequence of a mismatch is a rejected
    // login either way, but a token comparison is exactly the place where
    // short-circuiting on the first differing byte is a habit worth not forming.
    if !constant_time_eq(returned_state.as_bytes(), flow.csrf_state.as_bytes()) {
        tracing::warn!("OIDC callback state did not match; possible CSRF attempt");
        return Err(AppError::forbidden("this sign-in could not be verified"));
    }

    let code = params
        .code
        .ok_or_else(|| AppError::bad_request("the identity provider returned no code"))?;

    let identity = provider
        .complete(code, flow.pkce_verifier, flow.nonce)
        .await
        .map_err(|err| {
            tracing::warn!(error = ?err, "OIDC sign-in failed");
            // The message can be shown: it is either "not in the required group"
            // or a generic verification failure, neither of which leaks anything.
            AppError::forbidden(format!("Sign-in failed: {err}"))
        })?;

    let user = auth::provision_oidc_user(&state.pool, &identity).await?;
    let cookie = session::establish(&state, &user, web::user_agent(&headers).as_deref()).await?;

    let destination = flow.redirect_to.unwrap_or_else(|| "/".to_string());
    Ok((cleared.add(cookie), Redirect::to(&destination)).into_response())
}

fn flow_cookie(value: String, secure: bool) -> Cookie<'static> {
    let mut cookie = Cookie::new(FLOW_COOKIE, value);
    cookie.set_http_only(true);
    cookie.set_secure(secure);
    // Must be `Lax`, not `Strict`: the browser arrives at the callback as a
    // top-level navigation from the provider's origin, and `Strict` would
    // withhold the cookie exactly when it is needed.
    cookie.set_same_site(SameSite::Lax);
    cookie.set_path("/");
    cookie.set_max_age(time::Duration::minutes(10));
    cookie
}

fn clearing_flow_cookie(secure: bool) -> Cookie<'static> {
    let mut cookie = flow_cookie(String::new(), secure);
    cookie.set_max_age(time::Duration::seconds(0));
    cookie
}

/// Compares two byte strings without an early return on the first difference.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_comparison_agrees_with_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(constant_time_eq(b"", b""));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
        // Differing in the first byte must be no different from the last.
        assert!(!constant_time_eq(b"xbc", b"abc"));
    }

    #[test]
    fn flow_cookie_is_short_lived_and_protected() {
        let cookie = flow_cookie("id".into(), true);
        assert!(cookie.http_only().unwrap());
        assert!(cookie.secure().unwrap());
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert_eq!(cookie.max_age(), Some(time::Duration::minutes(10)));
    }
}
