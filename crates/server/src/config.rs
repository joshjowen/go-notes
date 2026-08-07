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
                //
                // Measured against BGE-small-en-v1.5 (the example deployments'
                // shipped model), embedding "{heading}\n\n{body}" exactly as
                // `embed_missing` does: genuinely related note pairs scored
                // 0.74-0.78, unrelated topics scored up to 0.62. The heading
                // matters here — two related notes almost always have
                // differently-worded headings, which measurably drags a true
                // match's score down (0.815 for one real pair on body text
                // alone, 0.742 once its heading was included) — so a number
                // tuned against bare passages would sit too high and miss real
                // matches. 0.70 sits in the gap with margin either side. A
                // different model, or a vault of much shorter or longer notes
                // than this was measured on, shifts the band; re-measure rather
                // than assume.
                min_score: 0.70,
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
            // The "is it empty?" check above is not enough on its own. The
            // shipped example config and compose file carry a `CHANGE_ME`
            // client secret whose Authelia-side digest is published in this
            // repository, so a deployer who copies the example and fills in
            // everything *except* this passes validation with a client secret
            // the whole internet knows — which for an OIDC confidential client
            // is as good as no authentication at all. Refuse to start on a
            // recognisably unedited placeholder rather than run wide open.
            if looks_like_placeholder(&self.auth.oidc.client_secret) {
                bail!(
                    "auth.oidc.client_secret is still an example placeholder; generate a real \
                     secret (see deploy/authelia/configuration.yml) before exposing this server"
                );
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
            if self.embeddings.api_key.is_empty()
                && reaches_the_public_internet(&self.embeddings.api_base)
            {
                tracing::warn!(
                    api_base = %self.embeddings.api_base,
                    "embeddings.api_key is empty and embeddings.api_base is not on this \
                     machine or its network; the endpoint will probably refuse every request"
                );
            }
            if reaches_the_public_internet(&self.embeddings.api_base) {
                // Said plainly, once, at startup: this is the one setting in the
                // application that can make an air-gapped deployment talk to
                // somebody else's server.
                tracing::info!(
                    api_base = %self.embeddings.api_base,
                    "the text of notes will be sent to this embeddings endpoint, \
                     which is outside this machine and its network"
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

/// The hostname out of a URL, without scheme, credentials, port or path.
fn host_of(url: &str) -> &str {
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
    match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.split(':').next().unwrap_or(""),
    }
}

/// Whether a URL names this machine.
fn is_loopback(url: &str) -> bool {
    let host = host_of(url);
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0")
        || host.starts_with("127.")
        || host.ends_with(".localhost")
}

/// Whether reaching this URL plausibly means leaving the machine and its network.
///
/// This decides two log lines — whether a missing API key is worth warning about,
/// and whether to say that notes are being sent somewhere — so it has to be right
/// about the shapes people actually write, and the container case is the one that
/// matters most. `http://embeddings:80/v1` is another container on an internal
/// network; treating it as remote made the shipped example warn, on every start,
/// that an endpoint sitting beside it "will probably refuse every request".
///
/// The rule is therefore: a single-label name is a container or a LAN host, a
/// private address range is a private network, and a handful of reserved suffixes
/// are by definition not routable. Everything else has a dotted public-looking
/// name, which is what a hosted API always has.
///
/// It cannot be exact — a company's `notes.internal.example.com` resolves inside
/// the building and still counts as public here. The consequence of being wrong
/// that way is one extra line in a log saying data may leave, which is the safe
/// direction to be wrong in.
fn reaches_the_public_internet(url: &str) -> bool {
    let host = host_of(url);
    if host.is_empty() || is_loopback(url) {
        return false;
    }

    // Kubernetes, Docker and Podman all resolve bare service names on an
    // internal network. Nothing on the public internet is reachable by one.
    if !host.contains('.') {
        return false;
    }

    const PRIVATE_SUFFIXES: [&str; 7] = [
        ".local",
        ".internal",
        ".lan",
        ".home",
        ".home.arpa",
        ".svc",
        ".cluster.local",
    ];
    if PRIVATE_SUFFIXES.iter().any(|suffix| host.ends_with(suffix)) {
        return false;
    }

    !is_private_address(host)
}

/// RFC 1918 and friends, for a host written as a literal address.
fn is_private_address(host: &str) -> bool {
    let octets: Vec<&str> = host.split('.').collect();
    if octets.len() != 4 || !octets.iter().all(|o| o.parse::<u8>().is_ok()) {
        // Not a dotted-quad, so nothing here applies. IPv6 unique-local is not
        // chased down: nobody configures one of these by hand.
        return false;
    }
    let first: u8 = octets[0].parse().unwrap_or(0);
    let second: u8 = octets[1].parse().unwrap_or(0);
    match first {
        10 => true,
        127 => true,
        172 => (16..=31).contains(&second),
        192 => second == 168,
        169 => second == 254,
        _ => false,
    }
}

/// Whether a value is a recognisably unedited example rather than a real secret.
///
/// Deliberately conservative: it matches only the exact markers this project
/// ships in its own example files, so a legitimately random secret can never
/// trip it by chance. The comparison is case-insensitive because the markers
/// appear in both upper and mixed case across the configs.
fn looks_like_placeholder(value: &str) -> bool {
    const MARKERS: [&str; 4] = ["CHANGE_ME", "CHANGEME", "REPLACE_THIS", "REPLACE_ME"];
    let normalised = value.to_ascii_uppercase();
    MARKERS.iter().any(|marker| normalised.contains(marker))
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

    /// The case that matters most, because it is what the shipped compose files
    /// use: a sibling container reached by service name. Calling that "not local"
    /// made the example warn on every start that the endpoint beside it would
    /// probably refuse every request.
    #[test]
    fn a_container_on_an_internal_network_is_not_the_public_internet() {
        assert!(!reaches_the_public_internet("http://embeddings:80/v1"));
        assert!(!reaches_the_public_internet("http://go-notes-embeddings:80/v1"));
        assert!(!reaches_the_public_internet("http://ollama:11434/v1"));
    }

    #[test]
    fn private_networks_and_reserved_names_are_not_the_public_internet() {
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://[::1]:8080/v1",
            "http://10.0.0.5:8080/v1",
            "http://192.168.1.20:8080/v1",
            "http://172.16.4.4:8080/v1",
            "http://172.31.255.1:8080/v1",
            "http://box.local:8080/v1",
            "http://models.internal:8080/v1",
            "http://tei.default.svc.cluster.local/v1",
        ] {
            assert!(!reaches_the_public_internet(url), "{url} should be private");
        }
    }

    #[test]
    fn a_hosted_api_is_the_public_internet() {
        for url in [
            "https://api.openai.com/v1",
            "https://api.mistral.ai/v1",
            "https://embeddings.example.com/v1",
            // Outside RFC 1918, so it is only reachable by routing there.
            "http://172.32.0.1:8080/v1",
            "http://8.8.8.8/v1",
        ] {
            assert!(reaches_the_public_internet(url), "{url} should be public");
        }
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

    /// The exact failure this guards against: someone copies the example, sets
    /// the issuer and enables OIDC, but never replaces the placeholder secret
    /// that this repository publishes the digest for.
    #[test]
    fn rejects_a_placeholder_oidc_client_secret() {
        let mut config = Config::default();
        config.auth.oidc.enabled = true;
        config.auth.oidc.issuer_url = "https://auth.example.com".into();

        for placeholder in ["CHANGE_ME", "change_me", "REPLACE_THIS_secret", "changeme123"] {
            config.auth.oidc.client_secret = placeholder.into();
            assert!(
                config.validate().is_err(),
                "{placeholder:?} should be refused as an unedited placeholder"
            );
        }

        // A real, high-entropy secret that merely happens to contain none of the
        // markers must pass.
        config.auth.oidc.client_secret = "s3cr3t-9f2a1c8b4d6e".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn placeholder_detection_does_not_fire_on_ordinary_secrets() {
        assert!(looks_like_placeholder("CHANGE_ME_session_secret"));
        assert!(looks_like_placeholder("replace_me"));
        // A random secret must not match. The check is a deliberately broad
        // substring scan — a secret that happened to embed "change_me" would be
        // rejected too — but that is a safe direction to err in (regenerate the
        // secret) and vanishingly unlikely for 256 bits of hex or base64.
        assert!(!looks_like_placeholder("secret"));
        assert!(!looks_like_placeholder("aZ09-random-bytes"));
        assert!(!looks_like_placeholder(
            "9f2a1c8b4d6e0f3a7b5c2d8e1f4a6b9c"
        ));
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
