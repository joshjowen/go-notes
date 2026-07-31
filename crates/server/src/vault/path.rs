//! The only place in the server that turns a client-supplied string into an
//! absolute path.
//!
//! Path safety is two gates. The first is syntactic and lives in
//! `go_notes_shared::paths`: it rejects `..`, absolute paths, dotfiles, control
//! characters and names that break on other operating systems. That alone
//! guarantees `root.join(rel)` cannot escape `root` *lexically*.
//!
//! The second gate is here, and it exists for the one escape the first cannot
//! see: a symlink. Someone with shell access to a vault could drop a symlink
//! pointing at `/etc` or at another user's vault, and a lexically-clean path
//! would then read straight through it. [`Vault::resolve`] walks every existing
//! ancestor and refuses if any of them is a link.
//!
//! On TOCTOU: a symlink swapped in between the check and the open would defeat
//! this. Doing better needs `openat(O_NOFOLLOW)` per component. We accept the
//! race deliberately, because winning it requires the ability to create files
//! inside the vault directory out-of-band — which means shell access as the
//! vault's owner, at which point the attacker can read those files directly and
//! has gained nothing. No API request can create a symlink.

use std::path::{Path, PathBuf};

use go_notes_shared::paths::{self, PathError};

use crate::error::{AppError, AppResult};

/// One user's notes directory.
#[derive(Debug, Clone)]
pub struct Vault {
    /// Absolute, canonicalised root. Canonicalising once at construction means
    /// the containment assertion below compares like with like.
    root: PathBuf,
}

/// A path that has passed both gates. Handlers accept this, never a `String`,
/// so it is not possible to reach the filesystem without validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultPath {
    /// Vault-relative, `/`-separated. Empty for the vault root.
    rel: String,
    abs: PathBuf,
}

impl VaultPath {
    /// Vault-relative path. This is what goes in the database and on the wire.
    pub fn rel(&self) -> &str {
        &self.rel
    }

    /// Absolute path on disk. Only the vault module should need this.
    pub fn abs(&self) -> &Path {
        &self.abs
    }

    pub fn basename(&self) -> &str {
        paths::basename(&self.rel)
    }

    pub fn parent_rel(&self) -> &str {
        paths::parent_of(&self.rel)
    }

    /// Filename with the extension removed — a note's default title.
    pub fn stem(&self) -> &str {
        paths::stem(&self.rel)
    }

    pub fn is_markdown(&self) -> bool {
        paths::has_md_extension(&self.rel)
    }

    pub fn extension(&self) -> Option<&str> {
        let name = self.basename();
        name.rsplit_once('.').map(|(_, ext)| ext).filter(|e| !e.is_empty())
    }
}

impl Vault {
    /// Opens a vault rooted at `data_dir/vault_dir`, creating it if needed.
    ///
    /// `vault_dir` comes from the database, where it was written after being
    /// passed through `sanitize_component`, so it is a single safe component.
    /// It is re-validated here rather than trusted, because "the database said
    /// so" is not a security property.
    pub fn open(data_dir: &Path, vault_dir: &str) -> AppResult<Vault> {
        paths::validate_component(vault_dir)
            .map_err(|_| AppError::Internal(anyhow::anyhow!("invalid vault directory name")))?;

        let root = data_dir.join(vault_dir);
        std::fs::create_dir_all(&root).map_err(|err| {
            AppError::Internal(anyhow::Error::new(err).context("creating vault directory"))
        })?;

        let root = root.canonicalize().map_err(|err| {
            AppError::Internal(anyhow::Error::new(err).context("canonicalising vault root"))
        })?;

        Ok(Vault { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolves a vault-relative path that must name a file.
    pub fn resolve(&self, rel: &str) -> AppResult<VaultPath> {
        paths::validate_rel_path(rel)?;
        self.build(rel)
    }

    /// Resolves a vault-relative path that must be a markdown note.
    pub fn resolve_note(&self, rel: &str) -> AppResult<VaultPath> {
        paths::validate_note_path(rel)?;
        self.build(rel)
    }

    /// Resolves a folder path; the empty string is the vault root.
    pub fn resolve_folder(&self, rel: &str) -> AppResult<VaultPath> {
        paths::validate_folder_path(rel)?;
        if rel.is_empty() {
            return Ok(VaultPath {
                rel: String::new(),
                abs: self.root.clone(),
            });
        }
        self.build(rel)
    }

    /// Resolves a path the *server* chose, bypassing the user-facing rules that
    /// forbid dotfiles.
    ///
    /// This is how `.trash/` is reached. It still enforces containment and the
    /// symlink walk; the only relaxation is the leading-dot rule, and it is not
    /// reachable from any request-derived string.
    pub(crate) fn resolve_internal(&self, rel: &str) -> AppResult<VaultPath> {
        if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') {
            return Err(AppError::InvalidPath(PathError::Absolute));
        }
        for component in rel.split('/') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(AppError::InvalidPath(PathError::ParentComponent));
            }
            if component.chars().any(|c| c.is_control()) {
                return Err(AppError::InvalidPath(PathError::ControlCharacter));
            }
        }
        self.build(rel)
    }

    fn build(&self, rel: &str) -> AppResult<VaultPath> {
        let abs = self.root.join(rel);

        // Defence in depth. Validation already makes this impossible, so a
        // failure here means a rule above has regressed.
        debug_assert!(abs.starts_with(&self.root));
        if !abs.starts_with(&self.root) {
            return Err(AppError::InvalidPath(PathError::ParentComponent));
        }

        self.reject_symlinked_ancestors(rel)?;
        Ok(VaultPath {
            rel: rel.to_string(),
            abs,
        })
    }

    /// Walks from the vault root down to the target, refusing if any component
    /// that exists is a symlink.
    ///
    /// Stops at the first component that does not exist: nothing can be nested
    /// under a path that is not there, so there is nothing further to check.
    fn reject_symlinked_ancestors(&self, rel: &str) -> AppResult<()> {
        let mut current = self.root.clone();
        for component in rel.split('/') {
            current.push(component);
            match std::fs::symlink_metadata(&current) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        tracing::warn!(
                            path = %current.display(),
                            "refusing to traverse a symlink inside a vault"
                        );
                        return Err(AppError::forbidden(
                            "that path passes through a symbolic link",
                        ));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(err) => {
                    return Err(AppError::Internal(
                        anyhow::Error::new(err).context("checking path for symlinks"),
                    ))
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn vault() -> (TempDir, Vault) {
        let dir = TempDir::new().unwrap();
        let vault = Vault::open(dir.path(), "alice").unwrap();
        (dir, vault)
    }

    #[test]
    fn resolves_a_plain_note() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Projects/Kitchen Reno.md").unwrap();
        assert_eq!(path.rel(), "Projects/Kitchen Reno.md");
        assert_eq!(path.basename(), "Kitchen Reno.md");
        assert_eq!(path.parent_rel(), "Projects");
        assert_eq!(path.stem(), "Kitchen Reno");
        assert_eq!(path.extension(), Some("md"));
        assert!(path.abs().starts_with(vault.root()));
    }

    #[test]
    fn root_resolves_only_as_a_folder() {
        let (_dir, vault) = vault();
        let root = vault.resolve_folder("").unwrap();
        assert_eq!(root.rel(), "");
        assert_eq!(root.abs(), vault.root());
        assert!(vault.resolve("").is_err());
    }

    #[test]
    fn rejects_traversal() {
        let (_dir, vault) = vault();
        for case in ["../alice2/note.md", "..", "a/../../b.md", "/etc/passwd"] {
            assert!(vault.resolve(case).is_err(), "{case} should be rejected");
        }
    }

    /// The escape the syntactic gate cannot see.
    #[test]
    fn refuses_to_read_through_a_symlinked_file() {
        let (dir, vault) = vault();
        let secret = dir.path().join("secret.md");
        fs::write(&secret, "another user's note").unwrap();

        std::os::unix::fs::symlink(&secret, vault.root().join("innocent.md")).unwrap();

        let err = vault.resolve_note("innocent.md").unwrap_err();
        assert!(matches!(err, AppError::Forbidden(_)), "got {err:?}");
    }

    #[test]
    fn refuses_to_traverse_a_symlinked_directory() {
        let (dir, vault) = vault();
        let outside = dir.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.md"), "secret").unwrap();

        std::os::unix::fs::symlink(&outside, vault.root().join("Shortcut")).unwrap();

        // Both the link itself and anything under it must be refused.
        assert!(matches!(
            vault.resolve_folder("Shortcut").unwrap_err(),
            AppError::Forbidden(_)
        ));
        assert!(matches!(
            vault.resolve_note("Shortcut/secret.md").unwrap_err(),
            AppError::Forbidden(_)
        ));
    }

    /// Creating a new note in a directory that does not exist yet has to work —
    /// the walk must stop at the first missing component rather than erroring.
    #[test]
    fn allows_paths_that_do_not_exist_yet() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("New/Deep/Folder/note.md").unwrap();
        assert!(!path.abs().exists());
    }

    #[test]
    fn internal_paths_reach_the_trash_but_still_refuse_traversal() {
        let (_dir, vault) = vault();
        let trash = vault.resolve_internal(".trash/deleted.md").unwrap();
        assert_eq!(trash.rel(), ".trash/deleted.md");

        // The user-facing entry point must not reach it.
        assert!(vault.resolve(".trash/deleted.md").is_err());

        // And the relaxed entry point still cannot climb out.
        assert!(vault.resolve_internal("../escape").is_err());
        assert!(vault.resolve_internal(".trash/../../escape").is_err());
    }

    #[test]
    fn vault_directory_name_is_revalidated() {
        let dir = TempDir::new().unwrap();
        // Even though this value would come from our own database, a traversal
        // in it must not open a vault outside the data directory.
        assert!(Vault::open(dir.path(), "../elsewhere").is_err());
        assert!(Vault::open(dir.path(), "a/b").is_err());
        assert!(Vault::open(dir.path(), "").is_err());
    }

    #[test]
    fn two_vaults_are_isolated() {
        let dir = TempDir::new().unwrap();
        let alice = Vault::open(dir.path(), "alice").unwrap();
        let bob = Vault::open(dir.path(), "bob").unwrap();
        fs::write(bob.root().join("private.md"), "bob's note").unwrap();

        // There is simply no string Alice can send that names Bob's file.
        for attempt in ["../bob/private.md", "..%2Fbob%2Fprivate.md", "/bob/private.md"] {
            assert!(alice.resolve(attempt).is_err(), "{attempt}");
        }
    }
}
