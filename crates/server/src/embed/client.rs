//! Talking to an OpenAI-compatible `/embeddings` endpoint.
//!
//! Deliberately the smallest thing that works against both shapes people
//! actually run: a model on localhost through Ollama or LM Studio, and a hosted
//! API that wants a bearer token. There is no default host, because guessing
//! wrong in either direction is worse than asking — a default of `localhost`
//! silently does nothing on a server that has no model, and a default of a
//! hosted API is an air-gapped deployment quietly dialling out.
//!
//! This is the only outbound HTTP in the server besides OIDC, and it copies two
//! things from `auth/oidc.rs`: redirects are refused, because following one to a
//! URL the configuration names turns this into an SSRF primitive against the
//! internal network, and both native and webpki roots are trusted, because
//! self-hosted endpoints sit behind internal CAs. It deliberately does *not*
//! copy the missing timeout — the OIDC client has none, which is survivable at
//! sign-in and is not survivable in a loop that runs forever.

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::config::EmbeddingsConfig;

pub struct EmbeddingClient {
    http: reqwest::Client,
    endpoint: String,
    api_key: String,
    model: String,
    dimensions: Option<u32>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

impl EmbeddingClient {
    /// Builds a client, or `None` when the feature is switched off.
    ///
    /// Returning `None` rather than an inert client is what makes "no model is
    /// configured" checkable: nothing constructs a `reqwest::Client` at all, and
    /// `the_embedding_client_is_not_built_when_it_is_disabled` in
    /// `tests/airgap.rs` holds it to that.
    pub fn new(config: &EmbeddingsConfig) -> Result<Option<EmbeddingClient>> {
        if !config.enabled {
            return Ok(None);
        }

        let http = reqwest::ClientBuilder::new()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .tls_built_in_native_certs(true)
            .tls_built_in_webpki_certs(true)
            .build()
            .context("building the embeddings HTTP client")?;

        Ok(Some(EmbeddingClient {
            http,
            endpoint: format!("{}/embeddings", config.api_base.trim_end_matches('/')),
            api_key: config.api_key.clone().into_inner(),
            model: config.model.clone(),
            dimensions: (config.dimensions > 0).then_some(config.dimensions),
        }))
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Embeds a batch, returning one vector per input in the same order.
    ///
    /// The order is restored from each datum's `index` rather than assumed:
    /// the specification says responses come back in order, and most servers do,
    /// but a mismatch here attaches a vector to the wrong passage and the only
    /// symptom is a graph full of edges nobody can explain.
    pub async fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let mut body = serde_json::json!({ "model": self.model, "input": inputs });
        if let Some(dimensions) = self.dimensions {
            body["dimensions"] = serde_json::json!(dimensions);
        }

        let mut request = self.http.post(&self.endpoint).json(&body);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }

        let response = request.send().await.context("calling the embeddings endpoint")?;
        let status = response.status();
        if !status.is_success() {
            // The body usually says which of the model name, the key or the
            // dimensions was wrong, and that is exactly what somebody reading
            // the log needs. Bounded, because an HTML error page is not a
            // useful log line.
            let detail = response.text().await.unwrap_or_default();
            let detail: String = detail.chars().take(300).collect();
            bail!("the embeddings endpoint returned {status}: {detail}");
        }

        let parsed: EmbeddingResponse = response
            .json()
            .await
            .context("reading the embeddings response")?;

        if parsed.data.len() != inputs.len() {
            bail!(
                "asked for {} embeddings and got {}",
                inputs.len(),
                parsed.data.len()
            );
        }

        let mut out = vec![Vec::new(); inputs.len()];
        for datum in parsed.data {
            let Some(slot) = out.get_mut(datum.index) else {
                bail!("the embeddings endpoint returned index {} out of range", datum.index);
            };
            *slot = datum.embedding;
        }
        if out.iter().any(Vec::is_empty) {
            bail!("the embeddings endpoint skipped an input");
        }

        Ok(out)
    }
}
