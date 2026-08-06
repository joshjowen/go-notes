//! Shared application state.

use std::sync::Arc;

use sqlx::PgPool;

use crate::auth::local::LocalUserStore;
use crate::auth::oidc::OidcProvider;
use crate::auth::throttle::LoginThrottle;
use crate::config::Config;
use crate::db::User;
use crate::error::AppResult;
use crate::vault::Vault;

/// Cheap to clone: every field is either a pool (already reference-counted) or
/// behind an `Arc`.
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: PgPool,
    /// `None` when local password login is disabled.
    pub local_users: Option<Arc<LocalUserStore>>,
    /// `None` when OIDC is disabled or discovery failed.
    pub oidc: Option<Arc<OidcProvider>>,
    pub throttle: Arc<LoginThrottle>,
    /// Whether an embeddings model is configured, so the frontend can leave the
    /// suggested-links control out on a server that can never produce any.
    pub semantic_links: bool,
}

impl AppState {
    /// Opens the vault belonging to a user.
    ///
    /// Constructed per request rather than cached: it is two `stat` calls, and a
    /// cache would have to be invalidated whenever a vault directory changed
    /// underneath it.
    pub fn vault_for(&self, user: &User) -> AppResult<Vault> {
        Vault::open(&self.config.server.data_dir, &user.vault_dir)
    }

    pub fn auth_info(&self) -> go_notes_shared::AuthInfo {
        go_notes_shared::AuthInfo {
            local_enabled: self.local_users.is_some(),
            oidc_button: self
                .oidc
                .as_ref()
                .map(|provider| provider.button_label.clone()),
        }
    }
}
