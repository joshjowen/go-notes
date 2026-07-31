//! Reading and writing the files in a vault.
//!
//! Two properties matter here and are worth stating outright.
//!
//! **Writes are atomic.** Every write goes to a temporary file in the same
//! directory, is flushed, and is then renamed over the target. A note is
//! therefore never observed half-written — not by a concurrent reader, not by
//! the filesystem watcher, and not by a `git commit` running over the vault
//! while the user is typing.
//!
//! **Deletes are recoverable.** Nothing is unlinked. Deleting moves the file
//! into `.trash/<timestamp>/`, preserving its original path underneath, so a
//! mis-click is undone with `mv`. The trash directory begins with a dot, which
//! the path rules make unaddressable from the API.

use std::path::Path;

use chrono::{DateTime, Utc};
use go_notes_shared::paths::{self, TRASH_DIR};
use tokio::io::AsyncWriteExt;
use walkdir::WalkDir;

use crate::error::{AppError, AppResult};
use crate::vault::path::{Vault, VaultPath};

/// A note as it exists on disk right now.
#[derive(Debug, Clone)]
pub struct NoteFile {
    pub markdown: String,
    /// blake3, hex-encoded. Doubles as the optimistic-concurrency token.
    pub content_hash: String,
    pub mtime: DateTime<Utc>,
    pub size_bytes: i64,
}

/// What a filesystem scan found, before any database comparison.
#[derive(Debug, Clone)]
pub struct ScannedFile {
    pub rel_path: String,
    pub mtime: DateTime<Utc>,
    pub size_bytes: i64,
}

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn mtime_of(meta: &std::fs::Metadata) -> DateTime<Utc> {
    meta.modified()
        .ok()
        .map(DateTime::<Utc>::from)
        // A filesystem without mtime support is not one we can reason about;
        // treating it as "now" makes reconciliation reindex, which is safe.
        .unwrap_or_else(Utc::now)
}

pub async fn read_note(path: &VaultPath) -> AppResult<NoteFile> {
    let bytes = tokio::fs::read(path.abs()).await?;
    let meta = tokio::fs::metadata(path.abs()).await?;

    // Notes are text by definition. A file that is not valid UTF-8 is either
    // corrupt or not really a note, and silently lossy-converting it would
    // destroy the user's data on the next save.
    let markdown = String::from_utf8(bytes).map_err(|_| {
        AppError::bad_request("this file is not valid UTF-8 text and cannot be opened as a note")
    })?;

    Ok(NoteFile {
        content_hash: hash_bytes(markdown.as_bytes()),
        size_bytes: meta.len() as i64,
        mtime: mtime_of(&meta),
        markdown,
    })
}

pub async fn exists(path: &VaultPath) -> bool {
    tokio::fs::symlink_metadata(path.abs()).await.is_ok()
}

pub async fn is_dir(path: &VaultPath) -> bool {
    tokio::fs::metadata(path.abs())
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

/// Writes `content` to `path`, creating parent directories as needed.
pub async fn write_note(path: &VaultPath, content: &str) -> AppResult<NoteFile> {
    if let Some(parent) = path.abs().parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    write_atomic(path.abs(), content.as_bytes()).await?;

    let meta = tokio::fs::metadata(path.abs()).await?;
    Ok(NoteFile {
        content_hash: hash_bytes(content.as_bytes()),
        size_bytes: meta.len() as i64,
        mtime: mtime_of(&meta),
        markdown: content.to_string(),
    })
}

/// Creates a note, failing if anything already exists at that path.
pub async fn create_note(path: &VaultPath, content: &str) -> AppResult<NoteFile> {
    if exists(path).await {
        return Err(AppError::AlreadyExists(format!(
            "'{}' already exists",
            path.rel()
        )));
    }
    write_note(path, content).await
}

/// Writes bytes to a temporary file and renames it into place.
///
/// The rename is what makes this atomic: on any POSIX filesystem it either
/// happens completely or not at all, so a reader sees the old file or the new
/// one and never a truncated mixture.
pub async fn write_atomic(target: &Path, bytes: &[u8]) -> AppResult<()> {
    let dir = target.parent().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!("refusing to write to a path with no parent"))
    })?;
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("note.md");

    // The temp name starts with a dot and ends in `.tmp`, so neither the tree
    // listing (which skips dotfiles) nor the watcher (which only reacts to `.md`)
    // will ever see it as a note.
    let temp = dir.join(format!(".{name}.{}.tmp", random_hex(8)));

    let write_result = async {
        let mut file = tokio::fs::File::create(&temp).await?;
        file.write_all(bytes).await?;
        // Without this the rename can land before the data does, leaving an
        // empty file after a crash or an abrupt container stop.
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, target).await?;
        Ok::<_, std::io::Error>(())
    }
    .await;

    if write_result.is_err() {
        // Best-effort: never leave litter behind on a failed write.
        let _ = tokio::fs::remove_file(&temp).await;
    }
    write_result?;
    Ok(())
}

/// Moves a note or folder. Refuses if the destination is occupied.
pub async fn move_entry(from: &VaultPath, to: &VaultPath) -> AppResult<()> {
    if !exists(from).await {
        return Err(AppError::NotFound);
    }
    if exists(to).await {
        return Err(AppError::AlreadyExists(format!(
            "'{}' already exists",
            to.rel()
        )));
    }
    // Moving a folder into itself would either fail cryptically or destroy the
    // subtree, depending on the filesystem.
    if paths::is_within(to.rel(), from.rel()) {
        return Err(AppError::bad_request(
            "cannot move a folder into one of its own subfolders",
        ));
    }

    if let Some(parent) = to.abs().parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(from.abs(), to.abs()).await?;
    Ok(())
}

pub async fn create_folder(path: &VaultPath) -> AppResult<()> {
    if exists(path).await {
        return Err(AppError::AlreadyExists(format!(
            "'{}' already exists",
            path.rel()
        )));
    }
    tokio::fs::create_dir_all(path.abs()).await?;
    Ok(())
}

/// Moves a note or folder into the vault's trash.
///
/// Returns the trash-relative location, which is what the API reports back so a
/// user can find the file again over SSH.
pub async fn move_to_trash(vault: &Vault, target: &VaultPath) -> AppResult<String> {
    if !exists(target).await {
        return Err(AppError::NotFound);
    }

    // Colons are legal on Linux but not on every filesystem a vault might later
    // be synced to, so the timestamp uses dashes throughout.
    let stamp = Utc::now().format("%Y-%m-%dT%H-%M-%S");
    let mut trash_rel = format!("{TRASH_DIR}/{stamp}/{}", target.rel());

    // Two deletions in the same second must not collide.
    if exists(&vault.resolve_internal(&trash_rel)?).await {
        trash_rel = format!(
            "{TRASH_DIR}/{stamp}-{}/{}",
            random_hex(4),
            target.rel()
        );
    }

    let destination = vault.resolve_internal(&trash_rel)?;
    if let Some(parent) = destination.abs().parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::rename(target.abs(), destination.abs()).await?;

    tracing::info!(from = %target.rel(), to = %trash_rel, "moved to trash");
    Ok(trash_rel)
}

/// Every markdown note in the vault, with the metadata reconciliation compares on.
///
/// Runs on a blocking thread: walking a large vault is thousands of `stat`
/// calls, which would otherwise stall the async runtime.
pub async fn scan_notes(vault: &Vault) -> AppResult<Vec<ScannedFile>> {
    let root = vault.root().to_path_buf();
    tokio::task::spawn_blocking(move || scan_dir(&root, |name| paths::has_md_extension(name)))
        .await
        .map_err(|err| AppError::Internal(anyhow::Error::new(err)))?
}

/// Every non-markdown file, i.e. the attachments.
pub async fn scan_attachments(vault: &Vault) -> AppResult<Vec<ScannedFile>> {
    let root = vault.root().to_path_buf();
    tokio::task::spawn_blocking(move || scan_dir(&root, |name| !paths::has_md_extension(name)))
        .await
        .map_err(|err| AppError::Internal(anyhow::Error::new(err)))?
}

/// Every directory in the vault, so empty folders still appear in the sidebar.
pub async fn scan_folders(vault: &Vault) -> AppResult<Vec<String>> {
    let root = vault.root().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut folders = Vec::new();
        for entry in WalkDir::new(&root)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !is_hidden(e))
        {
            let entry = match entry {
                Ok(entry) => entry,
                Err(err) => {
                    tracing::warn!(error = %err, "skipping unreadable directory entry");
                    continue;
                }
            };
            if entry.file_type().is_dir() {
                if let Some(rel) = relative_to(&root, entry.path()) {
                    folders.push(rel);
                }
            }
        }
        folders.sort();
        Ok(folders)
    })
    .await
    .map_err(|err| AppError::Internal(anyhow::Error::new(err)))?
}

fn scan_dir(root: &Path, accept: impl Fn(&str) -> bool) -> AppResult<Vec<ScannedFile>> {
    let mut found = Vec::new();

    for entry in WalkDir::new(root)
        .min_depth(1)
        // Never follow links out of the vault, matching the resolve-time rule.
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| !is_hidden(e))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                // One unreadable file must not abort the scan of an entire vault.
                tracing::warn!(error = %err, "skipping unreadable entry during scan");
                continue;
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if !accept(&name) {
            continue;
        }

        let Some(rel_path) = relative_to(root, entry.path()) else {
            continue;
        };
        // Anything the path rules would reject cannot be served over the API, so
        // there is no point indexing it. This is how a file with an illegal name
        // dropped in over SSH is handled: it stays on disk, it just stays invisible.
        if paths::validate_rel_path(&rel_path).is_err() {
            tracing::debug!(path = %rel_path, "ignoring file whose name is not addressable");
            continue;
        }

        let Ok(meta) = entry.metadata() else { continue };
        found.push(ScannedFile {
            rel_path,
            mtime: mtime_of(&meta),
            size_bytes: meta.len() as i64,
        });
    }

    Ok(found)
}

/// Skips dotfiles and dot-directories, which covers `.trash`, `.git`, `.obsidian`
/// and the temporary files written by [`write_atomic`].
fn is_hidden(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .is_some_and(|name| name.starts_with('.'))
}

/// Converts an absolute path back to a vault-relative, `/`-separated string.
fn relative_to(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            std::path::Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            // Anything else means the path is not the simple relative path we
            // expect, so refuse rather than guess.
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // Failure here means the OS has no entropy source, which is not a condition
    // we can meaningfully continue through.
    getrandom::fill(&mut buf).expect("system random number generator unavailable");
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn vault() -> (TempDir, Vault) {
        let dir = TempDir::new().unwrap();
        let vault = Vault::open(dir.path(), "alice").unwrap();
        (dir, vault)
    }

    #[tokio::test]
    async fn writes_and_reads_a_note() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Projects/Note.md").unwrap();

        let written = write_note(&path, "# Hello\n").await.unwrap();
        assert_eq!(written.size_bytes, 8);

        let read = read_note(&path).await.unwrap();
        assert_eq!(read.markdown, "# Hello\n");
        assert_eq!(read.content_hash, written.content_hash);
    }

    /// The bytes on disk must be exactly what was handed in — no added trailing
    /// newline, no normalised line endings. A vault under git would otherwise
    /// show spurious diffs after every save.
    #[tokio::test]
    async fn writes_bytes_verbatim() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Note.md").unwrap();

        for content in ["no trailing newline", "crlf\r\nline\r\n", "", "\n\n\n"] {
            write_note(&path, content).await.unwrap();
            let read = read_note(&path).await.unwrap();
            assert_eq!(read.markdown, content, "round-tripping {content:?}");
        }
    }

    #[tokio::test]
    async fn leaves_no_temporary_files_behind() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Note.md").unwrap();
        write_note(&path, "content").await.unwrap();

        let entries: Vec<_> = std::fs::read_dir(vault.root())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(entries, vec!["Note.md"]);
    }

    #[tokio::test]
    async fn refuses_to_create_over_an_existing_note() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Note.md").unwrap();
        create_note(&path, "first").await.unwrap();

        let err = create_note(&path, "second").await.unwrap_err();
        assert!(matches!(err, AppError::AlreadyExists(_)));
        // The original must be untouched.
        assert_eq!(read_note(&path).await.unwrap().markdown, "first");
    }

    #[tokio::test]
    async fn rejects_a_file_that_is_not_utf8() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Binary.md").unwrap();
        std::fs::write(path.abs(), [0xff, 0xfe, 0x00]).unwrap();

        assert!(matches!(
            read_note(&path).await.unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn moves_a_note() {
        let (_dir, vault) = vault();
        let from = vault.resolve_note("A.md").unwrap();
        let to = vault.resolve_note("Folder/B.md").unwrap();
        write_note(&from, "body").await.unwrap();

        move_entry(&from, &to).await.unwrap();
        assert!(!exists(&from).await);
        assert_eq!(read_note(&to).await.unwrap().markdown, "body");
    }

    #[tokio::test]
    async fn refuses_a_move_that_would_clobber() {
        let (_dir, vault) = vault();
        let from = vault.resolve_note("A.md").unwrap();
        let to = vault.resolve_note("B.md").unwrap();
        write_note(&from, "a").await.unwrap();
        write_note(&to, "b").await.unwrap();

        assert!(matches!(
            move_entry(&from, &to).await.unwrap_err(),
            AppError::AlreadyExists(_)
        ));
        assert_eq!(read_note(&to).await.unwrap().markdown, "b");
    }

    /// `mv Projects Projects/Sub` would otherwise lose the whole subtree.
    #[tokio::test]
    async fn refuses_to_move_a_folder_into_itself() {
        let (_dir, vault) = vault();
        let from = vault.resolve_folder("Projects").unwrap();
        let to = vault.resolve_folder("Projects/Nested").unwrap();
        create_folder(&from).await.unwrap();

        assert!(matches!(
            move_entry(&from, &to).await.unwrap_err(),
            AppError::BadRequest(_)
        ));
    }

    #[tokio::test]
    async fn deleting_moves_the_file_into_the_trash() {
        let (_dir, vault) = vault();
        let path = vault.resolve_note("Projects/Doomed.md").unwrap();
        write_note(&path, "still here").await.unwrap();

        let trash_rel = move_to_trash(&vault, &path).await.unwrap();
        assert!(!exists(&path).await);
        assert!(trash_rel.starts_with(".trash/"));
        // The original path is preserved under the timestamp, so it is obvious
        // where a file came from when restoring it.
        assert!(trash_rel.ends_with("/Projects/Doomed.md"));

        let recovered = vault.resolve_internal(&trash_rel).unwrap();
        assert_eq!(read_note(&recovered).await.unwrap().markdown, "still here");
    }

    #[tokio::test]
    async fn scanning_finds_notes_and_ignores_everything_else() {
        let (_dir, vault) = vault();
        write_note(&vault.resolve_note("A.md").unwrap(), "a")
            .await
            .unwrap();
        write_note(&vault.resolve_note("Folder/B.md").unwrap(), "b")
            .await
            .unwrap();

        // Things the scan must skip: trashed notes, dot-directories, and files
        // that are not markdown.
        move_to_trash(&vault, &vault.resolve_note("A.md").unwrap())
            .await
            .unwrap();
        std::fs::create_dir_all(vault.root().join(".obsidian")).unwrap();
        std::fs::write(vault.root().join(".obsidian/config.md"), "x").unwrap();
        std::fs::write(vault.root().join("image.png"), "x").unwrap();

        let mut found: Vec<_> = scan_notes(&vault)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.rel_path)
            .collect();
        found.sort();
        assert_eq!(found, vec!["Folder/B.md"]);

        let attachments: Vec<_> = scan_attachments(&vault)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.rel_path)
            .collect();
        assert_eq!(attachments, vec!["image.png"]);
    }

    /// A file dropped in over SSH with a name the API cannot address is left
    /// alone rather than half-indexed into something unreachable.
    #[tokio::test]
    async fn scanning_skips_names_the_api_could_never_serve() {
        let (_dir, vault) = vault();
        // All legal on Linux, none addressable through the API: a leading space,
        // a character Windows forbids, and a reserved device name.
        std::fs::write(vault.root().join(" leading space.md"), "x").unwrap();
        std::fs::write(vault.root().join("colon:name.md"), "x").unwrap();
        std::fs::write(vault.root().join("con.md"), "x").unwrap();
        std::fs::write(vault.root().join("fine.md"), "x").unwrap();

        let mut found: Vec<_> = scan_notes(&vault)
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.rel_path)
            .collect();
        found.sort();
        assert_eq!(found, vec!["fine.md"]);
    }

    #[tokio::test]
    async fn scanning_finds_empty_folders() {
        let (_dir, vault) = vault();
        create_folder(&vault.resolve_folder("Empty/Nested").unwrap())
            .await
            .unwrap();

        let folders = scan_folders(&vault).await.unwrap();
        assert_eq!(folders, vec!["Empty", "Empty/Nested"]);
    }

    #[tokio::test]
    async fn scanning_does_not_follow_symlinks_out_of_the_vault() {
        let (dir, vault) = vault();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret.md"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, vault.root().join("Shortcut")).unwrap();

        let found = scan_notes(&vault).await.unwrap();
        assert!(found.is_empty(), "found {found:?}");
    }
}
