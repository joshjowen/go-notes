//! The frontend must not reference anything outside its own origin.
//!
//! Go-Notes is meant to run on networks with no route to the internet at all. A
//! single `<link>` to a font service or a script from a CDN does not fail
//! loudly there — it hangs until the browser gives up, and the page renders in
//! the wrong typeface or, for a script, not at all. The server's
//! Content-Security-Policy already refuses those requests, but a policy
//! violation is a broken page too. The fix is to not have the reference.
//!
//! This test reads the frontend's sources rather than a build output, so it
//! runs with a plain `cargo test` and does not need Trunk or npm. It lives in
//! the server crate because that is the crate whose tests always run, and
//! because this binary is what serves those files.
//!
//! The check is deliberately narrow: it looks at the places a URL is *loaded
//! from* — `src=`, `href=`, `url(...)`, `@import` — rather than at any mention
//! of a URL, so documentation and comments are free to link wherever they like.

use std::path::{Path, PathBuf};

/// Files that decide what a browser fetches.
const FILES: [&str; 4] = [
    "crates/ui/index.html",
    "crates/ui/styles.css",
    "crates/ui/sw.js",
    "editor/src/theme.css",
];

fn repository_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is crates/server.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root is above the server crate")
}

/// Every absolute URL used as a load target, with the construct that used it.
fn external_references(source: &str) -> Vec<String> {
    let mut found = Vec::new();

    for (index, _) in source.match_indices("//") {
        // Only `//` that begins a scheme-relative or absolute URL.
        let before = &source[..index];
        let is_url = before.ends_with("http:")
            || before.ends_with("https:")
            || before.ends_with('=')
            || before.ends_with("=\"")
            || before.ends_with("='")
            || before.ends_with('(');
        if !is_url {
            continue;
        }

        // Which construct is this in? Look back a short way for one of the four
        // that make a browser fetch something.
        let window_start = index.saturating_sub(80);
        let window = &source[window_start..index];
        let loaded = ["src=", "href=", "url(", "@import"]
            .iter()
            .any(|construct| window.contains(construct));
        if !loaded {
            continue;
        }

        // Report the whole URL, scheme included, rather than from the `//`.
        let start = ["https:", "http:"]
            .iter()
            .find(|scheme| before.ends_with(*scheme))
            .map(|scheme| index - scheme.len())
            .unwrap_or(index);

        let url: String = source[start..]
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, '"' | '\'' | ')' | '>' | ';'))
            .collect();
        found.push(url);
    }

    found
}

/// A check that passes because it finds nothing is worth nothing unless it can
/// find something. These are the references it exists to catch.
#[test]
fn the_detector_catches_the_things_it_is_looking_for() {
    let font_link = r#"<link rel="stylesheet" href="https://fonts.googleapis.com/css?family=Inter" />"#;
    assert_eq!(
        external_references(font_link),
        vec!["https://fonts.googleapis.com/css?family=Inter".to_string()]
    );

    assert_eq!(
        external_references("<script src=\"//cdn.example.com/x.js\"></script>").len(),
        1
    );
    assert_eq!(
        external_references("@import url(https://example.com/theme.css);").len(),
        1
    );
    assert_eq!(
        external_references("body { background: url('http://example.com/bg.png'); }").len(),
        1
    );

    // And what it must not flag: local references, and prose.
    assert!(external_references(r#"<script src="/editor-bridge.js"></script>"#).is_empty());
    assert!(external_references("/* see https://example.com for why */").is_empty());
    assert!(external_references("url(data:image/png;base64,AAAA)").is_empty());
}

#[test]
fn the_frontend_loads_nothing_from_another_origin() {
    let root = repository_root();

    for file in FILES {
        let path = root.join(file);
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|err| panic!("could not read {}: {err}", path.display()));

        let external = external_references(&source);
        assert!(
            external.is_empty(),
            "{file} loads from another origin: {external:?}. Everything the page needs must be \
             served by go-notes itself, or an air-gapped deployment breaks."
        );
    }
}

/// The manifest is what makes the app installable, and an install pulls every
/// icon it names. One wrong path and the phone installs with a blank icon; one
/// remote URL and it does not install at all on a network with no internet.
#[test]
fn the_web_manifest_names_only_icons_that_are_in_the_bundle() {
    let root = repository_root();
    let manifest =
        std::fs::read_to_string(root.join("crates/ui/manifest.webmanifest")).expect("manifest");

    assert!(
        !manifest.contains("http://") && !manifest.contains("https://"),
        "the web manifest points at another origin"
    );

    let index = std::fs::read_to_string(root.join("crates/ui/index.html")).expect("index.html");
    let mut referenced: Vec<String> = Vec::new();
    for source in [&manifest, &index] {
        for (at, _) in source.match_indices("/icons/") {
            let name: String = source[at..]
                .chars()
                .take_while(|c| !matches!(c, '"' | '\'' | ' ' | '\n' | ')'))
                .collect();
            referenced.push(name);
        }
    }

    assert!(
        !referenced.is_empty(),
        "no icons referenced at all — the install would have no icon"
    );

    for icon in referenced {
        let path = root.join("crates/ui").join(icon.trim_start_matches('/'));
        assert!(
            path.exists(),
            "{} is referenced but not in crates/ui/icons — run crates/ui/render-icons.py",
            path.display()
        );
    }
}

/// The policy is the second half of the same guarantee: even a reference that
/// slipped past the check above must be refused at runtime.
#[test]
fn the_content_security_policy_names_no_external_origin() {
    let root = repository_root();
    let source = std::fs::read_to_string(root.join("crates/server/src/web.rs")).expect("web.rs");

    let policy_start = source.find("default-src 'self'").expect("the policy is in web.rs");
    let policy = &source[policy_start..policy_start + 900];

    assert!(
        !policy.contains("http://") && !policy.contains("https://"),
        "the Content-Security-Policy allows an external origin"
    );
    assert!(
        policy.contains("worker-src 'self'"),
        "the service worker needs worker-src; without it offline reloading silently stops working"
    );
}

/// The editor bundle is built from npm packages, and one of those adding a
/// webfont import would be invisible until someone opened the app somewhere
/// with no internet. Checked when the bundle exists — it is a build artifact,
/// so its absence is not a failure.
#[test]
fn the_built_editor_bundle_has_no_remote_imports() {
    let root = repository_root();
    let bundle = root.join("crates/ui/assets/editor-bridge.css");
    let Ok(source) = std::fs::read_to_string(&bundle) else {
        eprintln!("skipping: {} has not been built", bundle.display());
        return;
    };

    let external = external_references(&source);
    assert!(
        external.is_empty(),
        "the editor bundle fetches from another origin: {external:?}"
    );
}

// ---------------------------------------------------------------------------
// The server's own outbound connections
// ---------------------------------------------------------------------------
//
// Everything above is about what the *browser* fetches. Nothing was ever checked
// about what the server itself dials, which was fine while the only outbound
// call was OIDC — something you have to configure an issuer URL to get. The
// embeddings client is the second, and it is the first that could plausibly be
// left on by accident, so it gets a check of its own.

/// With embeddings disabled, no HTTP client is constructed at all.
///
/// Not "no request is made" — *no client exists*, which is a stronger and much
/// cheaper thing to assert. `EmbeddingClient::new` returning `None` is the only
/// path by which the worker is never spawned, so this is the single point where
/// "off" is decided.
#[test]
fn the_embedding_client_is_not_built_when_it_is_disabled() {
    let config = go_notes_server::config::Config::default();
    assert!(
        !config.embeddings.enabled,
        "embeddings must be off unless somebody turns them on"
    );

    let client = go_notes_server::embed::EmbeddingClient::new(&config.embeddings)
        .expect("building a disabled client cannot fail");
    assert!(
        client.is_none(),
        "a disabled embeddings config must produce no client"
    );
}

/// The other half, in the same spirit as `the_detector_catches_the_things_it_is
/// _looking_for`: prove the check above can fail, so that it passing means
/// something. A configuration that *is* enabled must produce a client.
#[test]
fn the_check_above_would_notice_a_client_being_built() {
    let mut config = go_notes_server::config::Config::default();
    config.embeddings.enabled = true;
    config.embeddings.api_base = "http://localhost:11434/v1".into();
    config.embeddings.model = "nomic-embed-text".into();

    let client = go_notes_server::embed::EmbeddingClient::new(&config.embeddings)
        .expect("building an enabled client");
    assert!(client.is_some());
}

/// Nothing in the shipped configuration points anywhere. A default that named a
/// host would mean an air-gapped deployment reaching for it the moment somebody
/// flipped `enabled`, having been told only that it turns a feature on.
#[test]
fn the_default_configuration_names_no_embeddings_host() {
    let config = go_notes_server::config::Config::default();
    assert_eq!(config.embeddings.api_base, "");
    assert_eq!(config.embeddings.model, "");
    assert!(config.embeddings.api_key.is_empty());
}
