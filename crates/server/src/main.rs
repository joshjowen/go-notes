//! Go-Notes — entry point and command-line interface.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use go_notes_server::auth::local::{hash_password, LocalUser, LocalUserStore, LocalUsers};
use go_notes_server::auth::oidc::OidcProvider;
use go_notes_server::auth::throttle::LoginThrottle;
use go_notes_server::config::Config;
use go_notes_server::state::AppState;
use go_notes_server::vault::{index, watch, Vault};
use go_notes_server::{db, routes};

#[derive(Parser)]
#[command(name = "go-notes", version, about = "Go-Notes — self-hosted markdown notes with a link graph")]
struct Cli {
    /// Path to config.toml. Every key can also be set as GO_NOTES__SECTION__KEY.
    #[arg(short, long, env = "GO_NOTES_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (the default).
    Serve,
    /// Check that the server is healthy. Used as the container HEALTHCHECK, so
    /// the image needs no curl.
    Healthcheck,
    /// Manage accounts in the local users file.
    #[command(subcommand)]
    User(UserCommand),
    /// End every browser session for a user, signing them out everywhere.
    ///
    /// Works for Authelia (OIDC) accounts as well as local ones, which is the
    /// point: removing someone upstream, or from the required group, does not
    /// reach go-notes' own sessions, so without this a deprovisioned user keeps
    /// access until their session simply expires. `user remove` only edits the
    /// local file and cannot touch an OIDC user at all.
    Logout {
        /// The username to sign out. Case-insensitive, as sign-in is.
        username: String,
    },
    /// Report where the index disagrees with the filesystem, without changing it.
    Check,
    /// Rebuild the index from the filesystem.
    Reindex,
    /// Embed any passages that have no vector yet, then recompute the semantic
    /// links between notes. Does nothing unless `[embeddings]` is enabled.
    Embed {
        /// Discard existing vectors and embed everything again. Only needed
        /// after changing the model, and it costs a full pass at the endpoint.
        #[arg(long)]
        all: bool,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Add a user, or change an existing user's password.
    Add {
        username: String,
        /// Read the password from this environment variable instead of prompting.
        #[arg(long)]
        password_env: Option<String>,
        #[arg(long)]
        display_name: Option<String>,
        #[arg(long)]
        email: Option<String>,
    },
    /// Change a password.
    Passwd {
        username: String,
        #[arg(long)]
        password_env: Option<String>,
    },
    /// List the accounts in the file.
    List,
    /// Remove an account from the file. The user's notes are left on disk.
    Remove { username: String },
    /// Print an argon2id hash, for pasting into a file by hand.
    Hash {
        #[arg(long)]
        password_env: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing();

    let config = Config::load(cli.config.as_deref())?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config).await,
        Command::Healthcheck => healthcheck(&config).await,
        Command::User(command) => user_command(&config, command),
        Command::Logout { username } => logout_user(config, username).await,
        Command::Check => reindex(config, true).await,
        Command::Reindex => reindex(config, false).await,
        Command::Embed { all } => embed_command(config, all).await,
    }
}

fn init_tracing() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // Quiet by default about the noisy dependencies, informative about us.
        EnvFilter::new("info,tower_http=warn,sqlx=warn,hyper=warn")
    });

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
}

async fn serve(config: Config) -> Result<()> {
    let config = Arc::new(config);

    std::fs::create_dir_all(&config.server.data_dir).with_context(|| {
        format!(
            "creating the data directory {}",
            config.server.data_dir.display()
        )
    })?;

    let pool = db::connect(&config.database).await?;
    db::migrate(&pool).await?;

    let local_users = if config.auth.local.enabled {
        // A store rather than a snapshot, so `go-notes user add` against a
        // running server takes effect without a restart.
        let store = LocalUserStore::new(&config.auth.local.users_file)?;
        tracing::info!(
            accounts = store.len(),
            path = %store.path().display(),
            "password sign-in enabled"
        );
        Some(Arc::new(store))
    } else {
        None
    };

    let oidc = if config.auth.oidc.enabled {
        // Discovery failure is fatal only if OIDC is the *only* way to sign in.
        // Otherwise the server still starts and the local login still works —
        // which is the whole reason both methods can be enabled at once.
        match OidcProvider::discover(&config.auth.oidc, &config.oidc_redirect_url()).await {
            Ok(provider) => Some(Arc::new(provider)),
            Err(err) if local_users.is_some() => {
                tracing::error!(
                    error = ?err,
                    "OIDC discovery failed; starting with password sign-in only"
                );
                None
            }
            Err(err) => {
                return Err(err
                    .context("OIDC discovery failed and it is the only enabled sign-in method"))
            }
        }
    } else {
        None
    };

    let state = AppState {
        config: config.clone(),
        pool: pool.clone(),
        local_users,
        oidc,
        throttle: Arc::new(LoginThrottle::new()),
        semantic_links: config.embeddings.enabled,
    };

    // Reconciliation runs in the background so a large vault does not delay the
    // server accepting requests. The index is a cache; serving from a slightly
    // stale one for a few seconds is better than not serving at all.
    if config.server.reconcile_on_start {
        let pool = pool.clone();
        let data_dir = config.server.data_dir.clone();
        tokio::spawn(async move {
            if let Err(err) = index::reconcile_all(&pool, &data_dir).await {
                tracing::error!(error = ?err, "startup reconcile failed");
            }
        });
    }

    // Held for the lifetime of the process: dropping it stops the watch.
    let _watcher = if config.server.watch_filesystem {
        Some(watch::spawn(pool.clone(), config.server.data_dir.clone())?)
    } else {
        tracing::info!("filesystem watching is disabled; external edits need a restart");
        None
    };

    spawn_session_cleanup(pool.clone());

    // Built here and spawned, never awaited before the listener binds: unlike
    // OIDC discovery, nothing about the application depends on this working, and
    // a model that is slow to answer must not be able to delay start-up. If the
    // endpoint is wrong the worker logs, backs off, and everything else carries
    // on with the semantic edges simply absent.
    match go_notes_server::embed::EmbeddingClient::new(&config.embeddings) {
        Ok(Some(client)) => {
            tracing::info!(model = %client.model(), "semantic links enabled");
            go_notes_server::embed::worker::spawn(pool.clone(), client, config.embeddings.clone());
        }
        Ok(None) => {}
        Err(err) => {
            tracing::error!(error = ?err, "could not build the embeddings client; \
                                           semantic links are off for this run");
        }
    }

    let app = routes::build(state);
    let addr: SocketAddr = config
        .server
        .bind
        .parse()
        .with_context(|| format!("invalid bind address '{}'", config.server.bind))?;

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;

    tracing::info!(
        address = %addr,
        public_url = %config.server.public_url,
        data_dir = %config.server.data_dir.display(),
        "go-notes is listening"
    );

    // `into_make_service_with_connect_info` is what makes the peer address
    // available to login throttling.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("server error")?;

    tracing::info!("shut down cleanly");
    Ok(())
}

/// Periodically clears expired sessions and abandoned login attempts.
fn spawn_session_cleanup(pool: sqlx::PgPool) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            ticker.tick().await;
            match go_notes_server::auth::session::purge_expired(&pool).await {
                Ok(count) if count > 0 => tracing::info!(count, "purged expired sessions"),
                Ok(_) => {}
                Err(err) => tracing::warn!(error = ?err, "session cleanup failed"),
            }
        }
    });
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c()
            .await
            .expect("installing the Ctrl-C handler");
    };

    // SIGTERM is what a container runtime sends on `podman stop`, so handling it
    // is the difference between shutting down cleanly and being killed once the
    // grace period runs out.
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(err) => {
                tracing::warn!(error = %err, "could not listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };

    tokio::select! {
        _ = interrupt => tracing::info!("received interrupt"),
        _ = terminate => tracing::info!("received terminate"),
    }
}

/// The container health check: does the server answer, and is the database up?
async fn healthcheck(config: &Config) -> Result<()> {
    let port = config
        .server
        .bind
        .rsplit_once(':')
        .map(|(_, port)| port)
        .unwrap_or("8080");

    let url = format!("http://127.0.0.1:{port}/healthz");
    let response = openidconnect::reqwest::Client::new()
        .get(&url)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;

    if response.status().is_success() {
        println!("ok");
        Ok(())
    } else {
        bail!("health check returned {}", response.status())
    }
}

/// Rebuilds the index from the filesystem, or just reports the drift.
///
/// This is the operational expression of "the files are the source of truth":
/// if the database is ever wrong, this is always the way to fix it.
/// One embedding pass, run to completion, with a report.
///
/// The same work the background worker does on its timer — this is for a first
/// run over an existing vault, where waiting for the interval to chew through it
/// is not what anybody wants to do.
async fn embed_command(config: Config, all: bool) -> Result<()> {
    let Some(client) = go_notes_server::embed::EmbeddingClient::new(&config.embeddings)? else {
        println!(
            "Embeddings are not enabled. Set embeddings.enabled, embeddings.api_base and \n\
             embeddings.model to point at an OpenAI-compatible endpoint."
        );
        return Ok(());
    };

    let pool = db::connect(&config.database).await?;
    db::migrate(&pool).await?;

    if all {
        // Only this model's rows: another model's vectors are not wrong, they
        // are simply for a different question, and throwing them away would
        // make switching back expensive for no reason.
        let cleared = sqlx::query("DELETE FROM embeddings WHERE model = $1")
            .bind(client.model())
            .execute(&pool)
            .await?
            .rows_affected();
        println!("Discarded {cleared} existing embeddings for {}.", client.model());
    }

    // A fresh `PassState`, so this always recomputes the links rather than
    // deciding nothing changed — somebody running the command by hand is asking
    // for exactly that.
    let mut state = go_notes_server::embed::worker::PassState::default();
    let report =
        go_notes_server::embed::worker::run_once(&pool, &client, &config.embeddings, &mut state)
            .await?;
    println!(
        "Embedded {} passages and wrote {} semantic links.",
        report.embedded, report.notes_relinked
    );
    if report.embedded == 0 {
        println!("Everything was already embedded.");
    }
    Ok(())
}

async fn reindex(config: Config, dry_run: bool) -> Result<()> {
    let pool = db::connect(&config.database).await?;
    db::migrate(&pool).await?;

    let users = db::list_users(&pool).await?;
    if users.is_empty() {
        println!("No users yet, so there is nothing to index.");
        return Ok(());
    }

    for user in users {
        let vault = Vault::open(&config.server.data_dir, &user.vault_dir)?;

        if dry_run {
            let drifted = index::drifted_paths(&pool, &user, &vault).await?;
            if drifted.is_empty() {
                println!("{}: index matches the filesystem", user.username);
            } else {
                println!(
                    "{}: {} indexed note(s) have no file on disk:",
                    user.username,
                    drifted.len()
                );
                for path in drifted {
                    println!("    {path}");
                }
            }
            continue;
        }

        let report = index::reconcile_vault(&pool, &user, &vault).await?;
        println!(
            "{}: {} added, {} updated, {} removed, {} failed",
            user.username, report.added, report.updated, report.removed, report.failed
        );
    }

    Ok(())
}

/// Ends every server-side session for a user.
///
/// Sessions live in Postgres, so unlike the file-only `user` subcommands this
/// has to connect. It looks the user up by name — reaching OIDC and local
/// accounts alike — and deletes their session rows, which takes effect on the
/// user's very next request because sessions are validated against the database
/// every time rather than trusted from the cookie.
async fn logout_user(config: Config, username: String) -> Result<()> {
    let pool = db::connect(&config.database).await?;
    db::migrate(&pool).await?;

    let Some(user) = db::find_user_by_username(&pool, &username).await? else {
        // Deliberately specific: a user only exists here once they have signed
        // in at least once, which is a common source of "but I added them".
        bail!(
            "no user '{username}' has signed in to this server yet, so there are no sessions to end"
        );
    };

    let ended = go_notes_server::auth::session::destroy_all_for_user(&pool, user.id).await?;
    match ended {
        0 => println!("'{}' had no active sessions.", user.username),
        1 => println!("Ended 1 session for '{}'.", user.username),
        n => println!("Ended {n} sessions for '{}'.", user.username),
    }
    if user.auth_provider == go_notes_server::auth::PROVIDER_OIDC {
        // The go-notes session is gone, but Authelia's is not: if they still
        // hold a valid Authelia session they can sign straight back in. Say so,
        // because the operator's intent when running this is usually to lock
        // someone out, not just to clear one of two doors.
        println!(
            "Note: this is an Authelia account. To keep them out, also remove them from the \
             required group (or disable the account) in Authelia — otherwise they can sign in \
             again immediately."
        );
    }
    Ok(())
}

fn user_command(config: &Config, command: UserCommand) -> Result<()> {
    let path = &config.auth.local.users_file;

    match command {
        UserCommand::Add {
            username,
            password_env,
            display_name,
            email,
        } => {
            let mut users = LocalUsers::load_or_empty(path)?;
            let existing = users.find(&username).cloned();
            let password = read_password(password_env.as_deref())?;

            users.upsert(LocalUser {
                display_name: display_name
                    .or_else(|| existing.as_ref().map(|u| u.display_name.clone()))
                    .unwrap_or_else(|| username.clone()),
                email: email.or_else(|| existing.as_ref().and_then(|u| u.email.clone())),
                password_hash: hash_password(&password)?,
                username: username.clone(),
            });
            users.save(path)?;

            if existing.is_some() {
                println!("Updated '{username}' in {}", path.display());
            } else {
                println!("Added '{username}' to {}", path.display());
                println!("Their vault directory is created the first time they sign in.");
            }
            Ok(())
        }

        UserCommand::Passwd {
            username,
            password_env,
        } => {
            let mut users = LocalUsers::load(path)?;
            let mut user = users
                .find(&username)
                .cloned()
                .with_context(|| format!("no user '{username}' in {}", path.display()))?;

            user.password_hash = hash_password(&read_password(password_env.as_deref())?)?;
            users.upsert(user);
            users.save(path)?;

            println!("Changed the password for '{username}'.");
            // Sessions live in Postgres, which this command deliberately does
            // not connect to. Saying so is better than implying they were ended.
            println!(
                "Existing browser sessions stay valid. To end them too, remove the \
                 account with `go-notes user remove` and add it again."
            );
            Ok(())
        }

        UserCommand::List => {
            let users = LocalUsers::load_or_empty(path)?;
            if users.users.is_empty() {
                println!("No accounts in {}", path.display());
                return Ok(());
            }
            for user in &users.users {
                match &user.email {
                    Some(email) => println!("{}  ({}, {email})", user.username, user.display_name()),
                    None => println!("{}  ({})", user.username, user.display_name()),
                }
            }
            Ok(())
        }

        UserCommand::Remove { username } => {
            let mut users = LocalUsers::load(path)?;
            if !users.remove(&username) {
                bail!("no user '{username}' in {}", path.display());
            }
            users.save(path)?;
            println!("Removed '{username}'. Their notes are still on disk.");
            Ok(())
        }

        UserCommand::Hash { password_env } => {
            println!("{}", hash_password(&read_password(password_env.as_deref())?)?);
            Ok(())
        }
    }
}

/// Reads a password from an environment variable, or by prompting.
///
/// There is deliberately no `--password` flag: an argument ends up in the shell
/// history and in `ps` output for every other user on the host.
fn read_password(env_var: Option<&str>) -> Result<String> {
    if let Some(name) = env_var {
        let value =
            std::env::var(name).with_context(|| format!("environment variable {name} is not set"))?;
        if value.is_empty() {
            bail!("environment variable {name} is empty");
        }
        return Ok(value);
    }

    let password = rpassword::prompt_password("Password: ")?;
    if password.len() < 8 {
        bail!("that password is shorter than 8 characters");
    }
    let again = rpassword::prompt_password("Confirm: ")?;
    if password != again {
        bail!("the passwords did not match");
    }
    Ok(password)
}
