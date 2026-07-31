//! Username and password authentication against a JSON file.
//!
//! This is the no-Authelia path: a single file of argon2id hashes, managed with
//! the `go-notes user` subcommands. It exists so the app is useful on its own,
//! without standing up an identity provider first.
//!
//! Passwords are hashed with argon2id at the parameters OWASP currently
//! recommends (19 MiB, 2 iterations, 1 lane). Argon2 rather than bcrypt or
//! PBKDF2 because its memory cost is what actually degrades GPU attacks; id
//! rather than i or d because it resists both side-channel and time-memory
//! tradeoff attacks.

use std::path::Path;

use anyhow::{bail, Context, Result};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::{Algorithm, Argon2, Params, Version};
use serde::{Deserialize, Serialize};

/// OWASP's second-choice argon2id parameters: 19 MiB of memory, 2 iterations,
/// 1 degree of parallelism. Chosen over the 46 MiB variant because a notes
/// server is frequently run on a small VPS or a home server, and 46 MiB per
/// concurrent login is enough to matter there.
const MEMORY_KIB: u32 = 19 * 1024;
const ITERATIONS: u32 = 2;
const PARALLELISM: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalUser {
    pub username: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub email: Option<String>,
    /// PHC-format argon2id string, e.g. `$argon2id$v=19$m=19456,t=2,p=1$...`
    pub password_hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LocalUsers {
    #[serde(default)]
    pub users: Vec<LocalUser>,
}

/// A precomputed hash of a value nobody knows, verified against when the
/// username does not exist.
///
/// Without this, a failed login for a real user takes ~50 ms (one argon2
/// verification) while a failed login for a nonexistent one returns instantly —
/// which is a reliable oracle for enumerating who has an account. Verifying
/// against a dummy hash makes both paths cost the same.
static DUMMY_HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn dummy_hash() -> &'static str {
    DUMMY_HASH.get_or_init(|| {
        hash_password("a password that is not any user's password")
            .expect("hashing the dummy password")
    })
}

fn argon2() -> Argon2<'static> {
    let params = Params::new(MEMORY_KIB, ITERATIONS, PARALLELISM, None)
        .expect("argon2 parameters are valid by construction");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = argon2()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|err| anyhow::anyhow!("hashing password: {err}"))?;
    Ok(hash.to_string())
}

impl LocalUsers {
    pub fn load(path: &Path) -> Result<LocalUsers> {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading local users file {}", path.display()))?;
        let users: LocalUsers = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing local users file {}", path.display()))?;
        users.validate()?;
        Ok(users)
    }

    /// Loads the file, or returns an empty set if it does not exist yet.
    ///
    /// A missing file is a normal state for a first run — the operator has not
    /// created any accounts yet — and failing to boot would leave them unable
    /// to reach the CLI that creates one.
    pub fn load_or_empty(path: &Path) -> Result<LocalUsers> {
        if !path.exists() {
            tracing::warn!(
                path = %path.display(),
                "no local users file yet; create an account with `go-notes user add`"
            );
            return Ok(LocalUsers::default());
        }
        LocalUsers::load(path)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for user in &self.users {
            if user.username.trim().is_empty() {
                bail!("a user in the local users file has an empty username");
            }
            if !seen.insert(user.username.to_lowercase()) {
                bail!("duplicate username '{}' in the local users file", user.username);
            }
            // Catching a plaintext password here is worth the check: a hand-edited
            // file with `"password_hash": "hunter2"` would otherwise fail every
            // login with no explanation of why.
            if PasswordHash::new(&user.password_hash).is_err() {
                bail!(
                    "user '{}' has a password_hash that is not a valid PHC string; \
                     generate one with `go-notes user add`",
                    user.username
                );
            }
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;

        // Written via a temp file and renamed, so an interrupted `user add`
        // cannot leave the operator locked out with a truncated file.
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, json.as_bytes() )?;
        restrict_permissions(&temp)?;
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    pub fn find(&self, username: &str) -> Option<&LocalUser> {
        self.users
            .iter()
            .find(|user| user.username.eq_ignore_ascii_case(username))
    }

    /// Checks a username and password.
    ///
    /// Always performs exactly one argon2 verification, whether or not the user
    /// exists, so the response time reveals nothing about which usernames are
    /// real.
    pub fn verify(&self, username: &str, password: &str) -> Option<&LocalUser> {
        let candidate = self.find(username);
        let hash_str = candidate
            .map(|user| user.password_hash.as_str())
            .unwrap_or_else(|| dummy_hash());

        let Ok(parsed) = PasswordHash::new(hash_str) else {
            tracing::error!(username, "stored password hash is unparseable");
            return None;
        };

        // Verification uses the parameters embedded in the stored hash, not the
        // constants above, so hashes written by an older version keep working.
        match argon2().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => candidate,
            Err(_) => None,
        }
    }

    pub fn upsert(&mut self, user: LocalUser) {
        match self
            .users
            .iter_mut()
            .find(|existing| existing.username.eq_ignore_ascii_case(&user.username))
        {
            Some(existing) => *existing = user,
            None => self.users.push(user),
        }
    }

    pub fn remove(&mut self, username: &str) -> bool {
        let before = self.users.len();
        self.users
            .retain(|user| !user.username.eq_ignore_ascii_case(username));
        self.users.len() != before
    }
}

/// The users file, reloaded when it changes on disk.
///
/// Without this, `go-notes user add` against a running server would appear to
/// succeed and then not work: the file would be updated but the process would go
/// on serving the copy it read at startup, and the new account could not sign in
/// until a restart. Since adding a user to a *running* container is exactly what
/// the documentation tells people to do, that would be a bad first experience.
///
/// The reload is guarded by the file's modification time, so the common case —
/// a login against an unchanged file — costs one `stat` rather than a reparse.
/// That is negligible beside the argon2 verification that follows it.
pub struct LocalUserStore {
    path: std::path::PathBuf,
    cache: std::sync::RwLock<Cached>,
}

struct Cached {
    modified: Option<std::time::SystemTime>,
    users: LocalUsers,
}

impl LocalUserStore {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Result<LocalUserStore> {
        let path = path.into();
        let users = LocalUsers::load_or_empty(&path)?;
        Ok(LocalUserStore {
            cache: std::sync::RwLock::new(Cached {
                modified: modified_time(&path),
                users,
            }),
            path,
        })
    }

    /// Number of accounts, for the startup log.
    pub fn len(&self) -> usize {
        self.cache
            .read()
            .map(|cache| cache.users.users.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Checks a username and password against the current contents of the file.
    pub fn verify(&self, username: &str, password: &str) -> Option<LocalUser> {
        self.refresh_if_changed();
        let cache = self.cache.read().ok()?;
        cache.users.verify(username, password).cloned()
    }

    fn refresh_if_changed(&self) {
        let on_disk = modified_time(&self.path);

        {
            let Ok(cache) = self.cache.read() else { return };
            if cache.modified == on_disk {
                return;
            }
        }

        match LocalUsers::load_or_empty(&self.path) {
            Ok(users) => {
                if let Ok(mut cache) = self.cache.write() {
                    tracing::info!(
                        path = %self.path.display(),
                        accounts = users.users.len(),
                        "reloaded the local users file"
                    );
                    cache.users = users;
                    cache.modified = on_disk;
                }
            }
            Err(err) => {
                // Keep serving the previous copy rather than locking everyone
                // out because someone saved a half-edited file.
                tracing::error!(
                    path = %self.path.display(),
                    error = ?err,
                    "could not reload the local users file; keeping the previous contents"
                );
            }
        }
    }
}

impl std::fmt::Debug for LocalUserStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalUserStore")
            .field("path", &self.path)
            .field("accounts", &self.len())
            .finish()
    }
}

fn modified_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// Makes the users file readable only by its owner.
///
/// The hashes are not plaintext, but they are still worth keeping out of reach
/// of other accounts on the host — argon2 or not, an offline attack on a weak
/// password is much easier than an online one.
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

impl LocalUser {
    pub fn display_name(&self) -> &str {
        if self.display_name.trim().is_empty() {
            &self.username
        } else {
            &self.display_name
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(username: &str, password: &str) -> LocalUser {
        LocalUser {
            username: username.into(),
            display_name: String::new(),
            email: None,
            password_hash: hash_password(password).unwrap(),
        }
    }

    #[test]
    fn hashes_are_argon2id_with_the_expected_parameters() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(hash.contains("m=19456,t=2,p=1"), "got {hash}");
    }

    /// Two users with the same password must not share a hash, or the file
    /// would leak which accounts have identical passwords.
    #[test]
    fn hashes_are_salted() {
        let a = hash_password("same").unwrap();
        let b = hash_password("same").unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn accepts_the_right_password_and_rejects_others() {
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "correct horse"));

        assert!(users.verify("josh", "correct horse").is_some());
        assert!(users.verify("josh", "wrong").is_none());
        assert!(users.verify("josh", "").is_none());
        assert!(users.verify("josh", "correct horse ").is_none());
    }

    #[test]
    fn usernames_are_case_insensitive() {
        let mut users = LocalUsers::default();
        users.upsert(user("Josh", "pw"));
        assert!(users.verify("josh", "pw").is_some());
        assert!(users.verify("JOSH", "pw").is_some());
    }

    /// The account-enumeration defence. An unknown username must not be
    /// distinguishable from a known one with the wrong password.
    #[test]
    fn unknown_users_still_cost_a_verification() {
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "pw"));

        let start = std::time::Instant::now();
        assert!(users.verify("nobody", "pw").is_none());
        let unknown = start.elapsed();

        let start = std::time::Instant::now();
        assert!(users.verify("josh", "wrong").is_none());
        let known = start.elapsed();

        // Not a timing assertion, which would be flaky; just a check that the
        // unknown-user path did real work rather than returning immediately.
        assert!(
            unknown.as_millis() > 1,
            "unknown-user lookup returned in {unknown:?}, which suggests the \
             dummy verification was skipped"
        );
        assert!(known.as_millis() > 1);
    }

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");

        let mut users = LocalUsers::default();
        users.upsert(LocalUser {
            display_name: "Josh Owen".into(),
            email: Some("josh@example.com".into()),
            ..user("josh", "pw")
        });
        users.save(&path).unwrap();

        let loaded = LocalUsers::load(&path).unwrap();
        assert_eq!(loaded.users.len(), 1);
        assert_eq!(loaded.users[0].display_name, "Josh Owen");
        assert!(loaded.verify("josh", "pw").is_some());
    }

    #[test]
    fn saved_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");

        let mut users = LocalUsers::default();
        users.upsert(user("josh", "pw"));
        users.save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode was {:o}", mode);
    }

    /// A hand-edited file with a plaintext password is a mistake people make;
    /// it must be reported clearly rather than failing every login silently.
    #[test]
    fn rejects_a_plaintext_password_in_the_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");
        std::fs::write(
            &path,
            r#"{"users":[{"username":"josh","password_hash":"hunter2"}]}"#,
        )
        .unwrap();

        let err = LocalUsers::load(&path).unwrap_err().to_string();
        assert!(err.contains("not a valid PHC string"), "got {err}");
    }

    #[test]
    fn rejects_duplicate_usernames() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");
        let hash = hash_password("pw").unwrap();
        std::fs::write(
            &path,
            format!(
                r#"{{"users":[
                    {{"username":"josh","password_hash":"{hash}"}},
                    {{"username":"JOSH","password_hash":"{hash}"}}
                ]}}"#
            ),
        )
        .unwrap();

        let err = LocalUsers::load(&path).unwrap_err().to_string();
        assert!(err.contains("duplicate username"), "got {err}");
    }

    #[test]
    fn a_missing_file_is_an_empty_set_rather_than_an_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let users = LocalUsers::load_or_empty(&dir.path().join("nope.json")).unwrap();
        assert!(users.users.is_empty());
        // And it must still be safe to attempt a login against it.
        assert!(users.verify("anyone", "anything").is_none());
    }

    #[test]
    fn upsert_replaces_rather_than_duplicating() {
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "old"));
        users.upsert(user("JOSH", "new"));

        assert_eq!(users.users.len(), 1);
        assert!(users.verify("josh", "new").is_some());
        assert!(users.verify("josh", "old").is_none());
    }

    /// The behaviour that makes `go-notes user add` work against a running
    /// container, which is exactly what the documentation tells people to do.
    #[test]
    fn the_store_picks_up_accounts_added_after_startup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");

        // Starts with no file at all — a first run.
        let store = LocalUserStore::new(&path).unwrap();
        assert!(store.is_empty());
        assert!(store.verify("josh", "pw").is_none());

        // Something else writes the file, as the CLI would.
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "pw"));
        users.save(&path).unwrap();
        bump_mtime(&path);

        assert!(
            store.verify("josh", "pw").is_some(),
            "the store should have noticed the new file"
        );
        assert_eq!(store.len(), 1);

        // And a password change is picked up too.
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "different"));
        users.save(&path).unwrap();
        bump_mtime(&path);

        assert!(store.verify("josh", "pw").is_none());
        assert!(store.verify("josh", "different").is_some());
    }

    /// A half-written file must not lock everyone out.
    #[test]
    fn the_store_keeps_the_last_good_copy_when_the_file_breaks() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("users.json");

        let mut users = LocalUsers::default();
        users.upsert(user("josh", "pw"));
        users.save(&path).unwrap();

        let store = LocalUserStore::new(&path).unwrap();
        assert!(store.verify("josh", "pw").is_some());

        std::fs::write(&path, "{ this is not json").unwrap();
        bump_mtime(&path);

        assert!(
            store.verify("josh", "pw").is_some(),
            "a broken file should not revoke everyone's access"
        );
    }

    /// Filesystem mtime granularity can be coarse enough that two writes in the
    /// same test share a timestamp; nudge it so the change is detectable.
    fn bump_mtime(path: &Path) {
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(2);
        let _ = filetime_set(path, later);
    }

    fn filetime_set(path: &Path, time: std::time::SystemTime) -> std::io::Result<()> {
        let file = std::fs::OpenOptions::new().write(true).open(path)?;
        file.set_modified(time)
    }

    #[test]
    fn remove_reports_whether_it_did_anything() {
        let mut users = LocalUsers::default();
        users.upsert(user("josh", "pw"));
        assert!(users.remove("JOSH"));
        assert!(!users.remove("josh"));
        assert!(users.users.is_empty());
    }
}
