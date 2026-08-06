//! Configuration, loaded from a TOML file and overridable by environment.
//!
//! Every key can be set as `GO_NOTES__<SECTION>__<KEY>`, e.g.
//! `GO_NOTES__DATABASE__URL`. The bare `DATABASE_URL` is also honoured because
//! that is what container orchestrators conventionally inject.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use figment::providers::{Env, Format, Serialized, Toml};
use figment::Figment;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub database: DatabaseConfig,
    pub auth: AuthConfig,
    pub uploads: UploadConfig,
    pub embeddings: EmbeddingsConfig,
}

/// A secret that does not print itself.
///
/// `Config` derives `Debug`, and one `tracing::debug!` of it would put an API
/// key in the log. Wrapping the value makes that impossible rather than merely
/// unlikely. `auth.oidc.client_secret` predates this and should adopt it too.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn into_inner(self) -> String {
        self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.0.is_empty() { "\"\"" } else { "\"***\"" })
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Secret(value)
    }
}

/// Semantic links, drawn from an OpenAI-compatible embeddings endpoint.
///
/// Off by default and with no host preset, because the two things people run —
/// a model on this machine and a hosted API — differ in whether using it means
/// anything leaves the network. Guessing either way would be wrong for the other.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingsConfig {
    pub enabled: bool,
    /// Base URL up to but not including `/embeddings`, e.g.
    /// `http://localhost:11434/v1` or `https://api.openai.com/v1`.
    pub api_base: String,
    /// Bearer token. Normally empty for a model running on this machine.
    pub api_key: Secret,
    pub model: String,
    /// Requested vector width; 0 leaves it to the model.
    pub dimensions: u32,
    /// Passages per request to the model.
    pub batch_size: usize,
    pub timeout_secs: u64,
    /// How often the worker looks for passages that have not been embedded.
    pub interval_secs: u64,
    /// Most semantic edges kept per note.
    pub neighbours: usize,
    /// Similarity below which two passages are not considered related.
    pub min_score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub bind: String,
    /// Externally visible origin, e.g. `https://notes.example.com`.
    ///
    /// Used to build the OIDC redirect URI and to validate the `Origin` header
    /// on mutating requests, so it must match what the browser actually sees.
    pub public_url: String,
    /// Root directory holding one subdirectory per user's vault.
    pub data_dir: PathBuf,
    /// Watch the data directory and reindex notes edited outside the app.
    pub watch_filesystem: bool,
    /// Rescan every vault against the database at startup.
    pub reconcile_on_start: bool,
    /// Read the client address from `X-Forwarded-For`.
    ///
    /// Only login throttling uses this, never authorisation. Leave it on behind
    /// a reverse proxy, where the socket address is always the proxy and every
    /// user would otherwise share one throttle bucket. Turn it off if the app is
    /// reachable directly, where the header is client-controlled.
    pub trust_proxy_headers: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub url: String,
    pub max_connections: u32,
    /// How long to keep retrying the initial connection. Compose and Quadlet
    /// both start containers before Postgres is ready to accept queries.
    pub connect_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    pub session_ttl_days: i64,
    /// Send the `Secure` cookie attribute. Only turn this off for plain-HTTP
    /// local development; without TLS a session cookie travels in the clear.
    pub cookie_secure: bool,
    pub local: LocalAuthConfig,
    pub oidc: OidcConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalAuthConfig {
    pub enabled: bool,
    /// JSON file of usernames and argon2id password hashes.
    pub users_file: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcConfig {
    pub enabled: bool,
    /// Base URL of the provider; discovery appends `/.well-known/openid-configuration`.
    pub issuer_url: String,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: Vec<String>,
    /// When set, only users whose `groups` claim contains this may sign in.
    pub required_group: Option<String>,
    /// Text on the sign-in button.
    pub button_label: String,
    /// Also end the session at the provider on logout (RP-initiated logout).
    pub end_session: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadConfig {
    pub max_bytes: usize,
    /// Extensions accepted for upload. The stored file's *sniffed* type must
    /// also be in the allowlist below — the declared name alone is never trusted.
    pub allowed_extensions: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            server: ServerConfig {
                bind: "0.0.0.0:8080".into(),
                public_url: "http://localhost:8080".into(),
                data_dir: PathBuf::from("/data/notes"),
                watch_filesystem: true,
                reconcile_on_start: true,
                trust_proxy_headers: true,
            },
            database: DatabaseConfig {
                url: "postgres://go_notes:go_notes@localhost/go_notes".into(),
                max_connections: 10,
                connect_timeout_secs: 60,
            },
            auth: AuthConfig {
                session_ttl_days: 30,
                cookie_secure: true,
                local: LocalAuthConfig {
                    enabled: true,
                    users_file: PathBuf::from("/config/users.json"),
                },
                oidc: OidcConfig {
                    enabled: false,
                    issuer_url: String::new(),
                    client_id: "go-notes".into(),
                    client_secret: String::new(),
                    scopes: vec![
                        "openid".into(),
                        "profile".into(),
                        "email".into(),
                        "groups".into(),
                    ],
                    required_group: None,
                    button_label: "Sign in with Authelia".into(),
                    end_session: true,
                },
            },
            embeddings: EmbeddingsConfig {
                enabled: false,
                api_base: String::new(),
                api_key: Secret::default(),
                model: String::new(),
                dimensions: 0,
                batch_size: 32,
                timeout_secs: 30,
                interval_secs: 60,
                neighbours: 5,
                // Chosen high on purpose. A threshold that is too low fills the
                // graph with edges between notes that merely share a register,
                // and a graph that claims everything is related says nothing.
                min_score: 0.78,
            },
            uploads: UploadConfig {
                max_bytes: 25 * 1024 * 1024,
                allowed_extensions: [
                    "png", "jpg", "jpeg", "gif", "webp", "avif", "svg", "pdf", "txt", "csv", "md",
                    "json", "zip", "mp3", "mp4", "webm", "ogg", "wav",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            },
        }
    }
}

impl Config {
    /// Loads defaults, then the TOML file if present, then the environment.
    ///
    /// A missing config file is not an error: a container configured purely
    /// through environment variables is a normal deployment.
    pub fn load(path: Option<&std::path::Path>) -> Result<Config> {
        let mut figment = Figment::from(Serialized::defaults(Config::default()));

        if let Some(path) = path {
            if path.exists() {
                figment = figment.merge(Toml::file(path));
            } else {
                bail!("config file {} does not exist", path.display());
            }
        }

        figment = figment.merge(Env::prefixed("GO_NOTES__").split("__"));

        // Conventional bare env vars, applied last so they win.
        if let Ok(url) = std::env::var("DATABASE_URL") {
            figment = figment.merge(Serialized::default("database.url", url));
        }

        let config: Config = figment.extract().context("invalid configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if !self.auth.local.enabled && !self.auth.oidc.enabled {
            bail!("no authentication method is enabled; set auth.local.enabled or auth.oidc.enabled");
        }
        if self.auth.oidc.enabled {
            if self.auth.oidc.issuer_url.is_empty() {
                bail!("auth.oidc.enabled is set but auth.oidc.issuer_url is empty");
            }
            if self.auth.oidc.client_secret.is_empty() {
                bail!("auth.oidc.enabled is set but auth.oidc.client_secret is empty");
            }
            if self.server.public_url.starts_with("http://localhost")
                || self.server.public_url.starts_with("http://127.")
            {
                tracing::warn!(
                    public_url = %self.server.public_url,
                    "OIDC is enabled with a localhost public_url; the redirect URI sent to the \
                     provider will not be reachable from other machines"
                );
            }
        }
        if self.embeddings.enabled {
            if self.embeddings.api_base.is_empty() {
                bail!("embeddings.enabled is set but embeddings.api_base is empty");
            }
            if self.embeddings.model.is_empty() {
                bail!("embeddings.enabled is set but embeddings.model is empty");
            }
            if self.embeddings.batch_size == 0 {
                bail!("embeddings.batch_size must be at least 1");
            }
            // A hosted endpoint with no key fails on the first request with a
            // 401 the user will find in a log an hour later; a local one with no
            // key is completely normal. Only one of those is worth a warning.
            if self.embeddings.api_key.is_empty() && !is_loopback(&self.embeddings.api_base) {
                tracing::warn!(
                    api_base = %self.embeddings.api_base,
                    "embeddings.api_key is empty and embeddings.api_base is not local; \
                     the endpoint will probably refuse every request"
                );
            }
            if !is_loopback(&self.embeddings.api_base) {
                // Said plainly, once, at startup: this is the one setting in the
                // application that can make an air-gapped deployment talk to
                // somebody else's server.
                tracing::info!(
                    api_base = %self.embeddings.api_base,
                    "embeddings will be sent to a non-local endpoint"
                );
            }
        }
        if self.server.public_url.ends_with('/') {
            bail!("server.public_url must not have a trailing slash");
        }
        if !self.auth.cookie_secure {
            tracing::warn!(
                "auth.cookie_secure is false; session cookies will be sent over plain HTTP"
            );
        }
        Ok(())
    }

    /// Where the OIDC provider sends the user back to.
    pub fn oidc_redirect_url(&self) -> String {
        format!("{}/api/auth/oidc/callback", self.server.public_url)
    }

    pub fn session_ttl(&self) -> chrono::Duration {
        chrono::Duration::days(self.auth.session_ttl_days)
    }
}

/// Whether a URL names this machine.
///
/// Only used to decide how loudly to talk about a missing API key and about
/// traffic leaving the host, so a hostname that merely resolves to a loopback
/// address is not chased down — the answer only has to be right for the shapes
/// people actually write.
fn is_loopback(url: &str) -> bool {
    let host = url
        .split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("");
    // A bracketed IPv6 literal keeps its colons, so the port can only be split
    // off after the closing bracket — `[::1]:8080` cut at the first colon leaves
    // `[`, which matches nothing and would have quietly called every local IPv6
    // endpoint remote.
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    };
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn rejects_having_no_way_to_log_in() {
        let mut config = Config::default();
        config.auth.local.enabled = false;
        config.auth.oidc.enabled = false;
        assert!(config.validate().is_err());
    }


    #[test]
    fn rejects_incomplete_embeddings_setup() {
        let mut config = Config::default();
        config.embeddings.enabled = true;
        assert!(
            config.validate().is_err(),
            "embeddings without an endpoint must not start"
        );

        config.embeddings.api_base = "http://localhost:11434/v1".into();
        assert!(
            config.validate().is_err(),
            "embeddings without a model must not start"
        );

        config.embeddings.model = "nomic-embed-text".into();
        assert!(config.validate().is_ok(), "a local model needs no API key");

        config.embeddings.batch_size = 0;
        assert!(
            config.validate().is_err(),
            "a batch size of zero would spin without ever sending anything"
        );
    }

    /// A hosted endpoint with no key is a mistake and a local one with no key is
    /// normal, so only one of them is worth saying anything about. Neither is
    /// fatal — the endpoint decides that, not this.
    #[test]
    fn a_missing_api_key_is_a_warning_and_never_an_error() {
        let mut config = Config::default();
        config.embeddings.enabled = true;
        config.embeddings.api_base = "https://api.openai.com/v1".into();
        config.embeddings.model = "text-embedding-3-small".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn loopback_is_recognised_in_the_shapes_people_write() {
        assert!(is_loopback("http://localhost:11434/v1"));
        assert!(is_loopback("http://127.0.0.1:1234/v1"));
        assert!(is_loopback("http://[::1]:8080/v1"));
        assert!(is_loopback("http://ollama.localhost/v1"));
        assert!(!is_loopback("https://api.openai.com/v1"));
        assert!(!is_loopback("https://localhost.example.com/v1"));
    }

    /// A secret that prints itself is a secret in a log file.
    #[test]
    fn a_secret_does_not_appear_in_debug_output() {
        let config = Config {
            embeddings: EmbeddingsConfig {
                api_key: Secret::from("sk-hunter2".to_string()),
                ..Config::default().embeddings
            },
            ..Config::default()
        };
        let printed = format!("{config:?}");
        assert!(!printed.contains("hunter2"), "the API key was printed");
        assert!(printed.contains("***"));
    }

    #[test]
    fn rejects_incomplete_oidc_setup() {
        let mut config = Config::default();
        config.auth.oidc.enabled = true;
        assert!(
            config.validate().is_err(),
            "OIDC without an issuer must not start"
        );

        config.auth.oidc.issuer_url = "https://auth.example.com".into();
        assert!(
            config.validate().is_err(),
            "OIDC without a client secret must not start"
        );

        config.auth.oidc.client_secret = "secret".into();
        config.validate().unwrap();
    }

    #[test]
    fn rejects_trailing_slash_in_public_url() {
        let mut config = Config::default();
        config.server.public_url = "https://notes.example.com/".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn builds_redirect_url() {
        let mut config = Config::default();
        config.server.public_url = "https://notes.example.com".into();
        assert_eq!(
            config.oidc_redirect_url(),
            "https://notes.example.com/api/auth/oidc/callback"
        );
    }
}
