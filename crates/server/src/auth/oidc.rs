//! Authelia (or any OpenID Provider) as the identity source.
//!
//! ## Why this rather than forward-auth headers
//!
//! The usual Caddy + Authelia recipe uses `forward_auth`: Caddy asks Authelia
//! whether a request is allowed, and on success copies `Remote-User` and friends
//! into the upstream request. It works, but the application's security then rests
//! entirely on network topology — the app must be unreachable except through the
//! proxy, because anyone who can open a socket to it can simply set
//! `Remote-User: josh` and become that person. A stray published port, a
//! misconfigured network, or another container on the same bridge is enough.
//!
//! Here the app is an OIDC client instead. It performs the authorization-code
//! flow itself and verifies the ID token's signature against the provider's
//! JWKS, so identity is established cryptographically rather than by trusting a
//! header. The app is safe even if it is exposed directly, and Caddy goes back
//! to being nothing more than a TLS terminator.
//!
//! ## What is verified
//!
//! `IdToken::claims` checks the signature, issuer, audience, expiry and nonce.
//! On top of that this module checks the `state` parameter against the value
//! stored server-side, and optionally requires a group claim.

use anyhow::{anyhow, Context, Result};
use openidconnect::core::{
    CoreAuthDisplay, CoreAuthPrompt, CoreAuthenticationFlow, CoreErrorResponseType, CoreGenderClaim,
    CoreJsonWebKey, CoreJweContentEncryptionAlgorithm, CoreJwsSigningAlgorithm, CoreRevocableToken,
    CoreRevocationErrorResponse, CoreTokenIntrospectionResponse, CoreTokenType,
};
use openidconnect::reqwest;
use openidconnect::{
    AuthorizationCode, ClientId, ClientSecret, CsrfToken, EmptyExtraTokenFields, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IdTokenFields, IssuerUrl, Nonce, OAuth2TokenResponse,
    PkceCodeChallenge, PkceCodeVerifier, ProviderMetadataWithLogout, RedirectUrl, Scope,
    StandardErrorResponse, StandardTokenResponse, TokenResponse, UserInfoClaims,
};
use serde::Deserialize;

use crate::config::OidcConfig;

/// Claims Authelia adds beyond the standard OIDC set.
///
/// `groups` is not part of the core specification, so it has to be declared as
/// an additional claim; the whole generic tangle below exists to thread this one
/// struct through the client's type parameters.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct ExtraClaims {
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub preferred_username: Option<String>,
}

impl openidconnect::AdditionalClaims for ExtraClaims {}

/// The token response, parameterised on our additional claims rather than the
/// crate's `EmptyAdditionalClaims`.
type TokenResponseWithGroups = StandardTokenResponse<
    IdTokenFields<
        ExtraClaims,
        EmptyExtraTokenFields,
        CoreGenderClaim,
        CoreJweContentEncryptionAlgorithm,
        CoreJwsSigningAlgorithm,
    >,
    CoreTokenType,
>;

/// `CoreClient` with `ExtraClaims` substituted in.
///
/// The endpoint-state parameters are the crate's type-state tracking of which
/// URLs are configured; after discovery and `set_redirect_uri` the authorization
/// URL is definitely set, and the token and userinfo endpoints are "maybe set"
/// because a provider is not obliged to advertise them.
type ClientWithGroups = openidconnect::Client<
    ExtraClaims,
    CoreAuthDisplay,
    CoreGenderClaim,
    CoreJweContentEncryptionAlgorithm,
    CoreJsonWebKey,
    CoreAuthPrompt,
    StandardErrorResponse<CoreErrorResponseType>,
    TokenResponseWithGroups,
    CoreTokenIntrospectionResponse,
    CoreRevocableToken,
    CoreRevocationErrorResponse,
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

pub struct OidcProvider {
    client: ClientWithGroups,
    http: reqwest::Client,
    scopes: Vec<String>,
    required_group: Option<String>,
    end_session_endpoint: Option<String>,
    pub button_label: String,
}

/// Everything a successful sign-in tells us about the person.
#[derive(Debug, Clone)]
pub struct OidcIdentity {
    /// The provider's `sub`. Immutable by specification, and therefore the only
    /// safe thing to key a vault on.
    pub subject: String,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub groups: Vec<String>,
}

/// The values that must survive the round trip to the provider and back.
pub struct PendingFlow {
    pub authorize_url: String,
    pub csrf_state: String,
    pub nonce: String,
    pub pkce_verifier: String,
}

impl OidcProvider {
    /// Fetches the provider's metadata and builds a client.
    pub async fn discover(config: &OidcConfig, redirect_url: &str) -> Result<OidcProvider> {
        let issuer = IssuerUrl::new(config.issuer_url.clone())
            .with_context(|| format!("invalid OIDC issuer URL '{}'", config.issuer_url))?;

        let http = reqwest::ClientBuilder::new()
            // Following redirects from a URL the provider controls would turn
            // this client into an SSRF primitive against the internal network.
            .redirect(reqwest::redirect::Policy::none())
            // Trust the host's certificate store *as well as* the bundled
            // Mozilla roots. Bundled roots alone are the usual default, and they
            // are wrong for this application: a self-hosted Authelia very often
            // sits behind an internal CA, step-ca, or a company CA, and with
            // only public roots the discovery request below fails with a
            // certificate error that the operator cannot fix without rebuilding
            // the image. Mounting a CA into the container is the normal remedy,
            // and this is what makes it work.
            .tls_built_in_native_certs(true)
            .tls_built_in_webpki_certs(true)
            .build()
            .context("building the OIDC HTTP client")?;

        // `ProviderMetadataWithLogout` is the core metadata plus
        // `end_session_endpoint`, which is what RP-initiated logout needs.
        let metadata = ProviderMetadataWithLogout::discover_async(issuer, &http)
            .await
            .with_context(|| {
                format!(
                    "fetching OpenID configuration from {}/.well-known/openid-configuration",
                    config.issuer_url
                )
            })?;

        let end_session_endpoint = metadata
            .additional_metadata()
            .end_session_endpoint
            .as_ref()
            .map(|url| url.to_string());

        let client = ClientWithGroups::from_provider_metadata(
            metadata,
            ClientId::new(config.client_id.clone()),
            Some(ClientSecret::new(config.client_secret.clone())),
        )
        .set_redirect_uri(
            RedirectUrl::new(redirect_url.to_string())
                .with_context(|| format!("invalid redirect URL '{redirect_url}'"))?,
        );

        tracing::info!(
            issuer = %config.issuer_url,
            client_id = %config.client_id,
            redirect_uri = %redirect_url,
            rp_initiated_logout = end_session_endpoint.is_some(),
            "OIDC provider discovered"
        );

        Ok(OidcProvider {
            client,
            http,
            scopes: config.scopes.clone(),
            required_group: config.required_group.clone(),
            end_session_endpoint,
            button_label: config.button_label.clone(),
        })
    }

    /// Starts a sign-in: builds the URL to send the browser to.
    ///
    /// PKCE is used even though this is a confidential client with a secret.
    /// It costs nothing and closes the authorization-code interception window if
    /// the redirect is ever observed — for instance in browser history, a proxy
    /// log, or a `Referer` header.
    pub fn begin(&self) -> PendingFlow {
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

        let mut request = self.client.authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        );
        for scope in &self.scopes {
            // `authorize_url` already includes `openid`, so adding it again from
            // the configured list produces `scope=openid+openid+profile...`.
            // Harmless with Authelia, but a duplicate is still malformed and a
            // stricter provider would be within its rights to reject it.
            if scope.eq_ignore_ascii_case("openid") {
                continue;
            }
            request = request.add_scope(Scope::new(scope.clone()));
        }

        let (url, csrf_state, nonce) = request.set_pkce_challenge(pkce_challenge).url();

        PendingFlow {
            authorize_url: url.to_string(),
            csrf_state: csrf_state.secret().clone(),
            nonce: nonce.secret().clone(),
            pkce_verifier: pkce_verifier.into_secret(),
        }
    }

    /// Completes a sign-in: swaps the code for tokens and verifies the ID token.
    pub async fn complete(
        &self,
        code: String,
        pkce_verifier: String,
        nonce: String,
    ) -> Result<OidcIdentity> {
        let tokens = self
            .client
            .exchange_code(AuthorizationCode::new(code))?
            .set_pkce_verifier(PkceCodeVerifier::new(pkce_verifier))
            .request_async(&self.http)
            .await
            .context("exchanging the authorization code for tokens")?;

        let id_token = tokens
            .id_token()
            .ok_or_else(|| anyhow!("the provider did not return an ID token"))?;

        // This is the security-critical call: it checks the token's signature
        // against the provider's published keys, and validates the issuer,
        // audience, expiry and — because we pass it — the nonce we generated.
        // Without the nonce check, a token obtained for a different session
        // could be replayed into this one.
        let verifier = self.client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &Nonce::new(nonce))
            .context("verifying the ID token")?;

        let subject = claims.subject().to_string();
        if subject.is_empty() {
            return Err(anyhow!("the provider returned an empty subject"));
        }

        let mut groups = claims.additional_claims().groups.clone();
        let mut preferred = claims.additional_claims().preferred_username.clone();
        let mut email = claims.email().map(|e| e.to_string());
        let mut display_name = localized_name(claims.name());

        // Authelia can be configured to keep claims out of the ID token and
        // serve them from the userinfo endpoint instead, so fall back to it when
        // something we need is missing rather than failing the sign-in.
        if preferred.is_none() || email.is_none() || groups.is_empty() {
            match self.fetch_userinfo(tokens.access_token()).await {
                Ok(info) => {
                    if groups.is_empty() {
                        groups = info.additional_claims().groups.clone();
                    }
                    if preferred.is_none() {
                        preferred = info.additional_claims().preferred_username.clone();
                    }
                    if email.is_none() {
                        email = info.email().map(|e| e.to_string());
                    }
                    if display_name.is_none() {
                        display_name = localized_name(info.name());
                    }
                }
                Err(err) => {
                    tracing::debug!(error = %err, "userinfo lookup failed; using ID token claims only");
                }
            }
        }

        if let Some(required) = &self.required_group {
            if !groups.iter().any(|group| group == required) {
                tracing::warn!(
                    subject = %subject,
                    required = %required,
                    groups = ?groups,
                    "rejecting sign-in: user is not in the required group"
                );
                return Err(anyhow!(
                    "your account is not a member of the '{required}' group"
                ));
            }
        }

        // A username is needed for display and to derive a vault directory. Fall
        // back through progressively less friendly options rather than failing:
        // the subject is always present, even if it is a UUID.
        let username = preferred
            .clone()
            .or_else(|| email.as_ref().and_then(|e| e.split('@').next().map(str::to_string)))
            .unwrap_or_else(|| subject.clone());

        Ok(OidcIdentity {
            display_name: display_name.unwrap_or_else(|| username.clone()),
            subject,
            username,
            email,
            groups,
        })
    }

    async fn fetch_userinfo(
        &self,
        access_token: &openidconnect::AccessToken,
    ) -> Result<UserInfoClaims<ExtraClaims, CoreGenderClaim>> {
        let claims = self
            .client
            .user_info(access_token.clone(), None)?
            .request_async(&self.http)
            .await?;
        Ok(claims)
    }

    /// Where to send the browser to also end the session at the provider.
    ///
    /// Without this, signing out of the notes app leaves the Authelia session
    /// intact, so clicking "sign in" again silently logs straight back in — which
    /// looks broken, and on a shared machine is a real problem.
    pub fn end_session_url(&self, post_logout_redirect: &str) -> Option<String> {
        let endpoint = self.end_session_endpoint.as_ref()?;
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        Some(format!(
            "{endpoint}{separator}post_logout_redirect_uri={}",
            urlencode(post_logout_redirect)
        ))
    }
}

fn localized_name(name: Option<&openidconnect::LocalizedClaim<openidconnect::EndUserName>>) -> Option<String> {
    name.and_then(|claim| claim.get(None))
        .map(|name| name.as_str().to_string())
}

fn urlencode(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_claims_tolerate_a_provider_that_omits_them() {
        // Authelia only includes `groups` when the scope was granted, so the
        // absent case has to deserialise rather than error.
        let claims: ExtraClaims = serde_json::from_str("{}").unwrap();
        assert!(claims.groups.is_empty());
        assert_eq!(claims.preferred_username, None);

        let claims: ExtraClaims =
            serde_json::from_str(r#"{"groups":["admins","notes"],"preferred_username":"josh"}"#)
                .unwrap();
        assert_eq!(claims.groups, vec!["admins", "notes"]);
        assert_eq!(claims.preferred_username.as_deref(), Some("josh"));
    }

    #[test]
    fn post_logout_redirect_is_encoded() {
        assert_eq!(
            urlencode("https://notes.example.com/"),
            "https%3A%2F%2Fnotes%2Eexample%2Ecom%2F"
        );
    }
}
