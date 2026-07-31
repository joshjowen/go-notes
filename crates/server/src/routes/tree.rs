//! The sidebar: the folder tree, and the operations that reshape it.
//!
//! Folder operations are real filesystem operations. Dragging a note into a
//! folder in the sidebar runs `rename(2)`; creating a group creates a directory.
//! The sidebar is a view of the filesystem, not a database structure that
//! happens to resemble one, so what a user sees in the browser is exactly what
//! they find over SSH.

use std::collections::BTreeMap;

use axum::extract::{Path, State};
use axum::response::Response;
use axum::Json;
use go_notes_shared::paths;
use go_notes_shared::{CreateFolderRequest, FolderStateRequest, MoveRequest, MoveResponse, TreeNode};
use sqlx::Row;

use crate::auth::session::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::vault::{index, store};
use crate::web;

/// A flat row of the tree, before it is nested.
struct Entry {
    path: String,
    title: String,
    is_folder: bool,
    collapsed: bool,
}

pub async fn tree(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> AppResult<Json<TreeNode>> {
    let notes = sqlx::query("SELECT rel_path, title FROM notes WHERE user_id = $1")
        .bind(user.id)
        .fetch_all(&state.pool)
        .await?;

    let folders = sqlx::query("SELECT rel_path, collapsed FROM folders WHERE user_id = $1")
        .bind(user.id)
        .fetch_all(&state.pool)
        .await?;

    let mut entries = Vec::with_capacity(notes.len() + folders.len());
    for row in folders {
        entries.push(Entry {
            path: row.try_get("rel_path")?,
            title: String::new(),
            is_folder: true,
            collapsed: row.try_get("collapsed")?,
        });
    }
    for row in notes {
        entries.push(Entry {
            path: row.try_get("rel_path")?,
            title: row.try_get("title")?,
            is_folder: false,
            collapsed: false,
        });
    }

    Ok(Json(build_tree(entries)))
}

/// Nests a flat list of paths into a tree.
///
/// Folders are materialised from note paths as well as from the `folders` table,
/// so a tree is still correct if the folder rows are somehow behind — the note
/// paths alone contain the whole structure.
fn build_tree(entries: Vec<Entry>) -> TreeNode {
    // Sorted maps throughout, so the sidebar order is stable and alphabetical
    // rather than dependent on however Postgres happened to return the rows.
    struct Dir {
        collapsed: bool,
        subdirs: BTreeMap<String, Dir>,
        notes: BTreeMap<String, (String, String)>,
    }

    impl Dir {
        fn new() -> Dir {
            Dir {
                collapsed: false,
                subdirs: BTreeMap::new(),
                notes: BTreeMap::new(),
            }
        }

        fn descend(&mut self, components: &[&str]) -> &mut Dir {
            let mut current = self;
            for component in components {
                current = current
                    .subdirs
                    .entry((*component).to_string())
                    .or_insert_with(Dir::new);
            }
            current
        }
    }

    let mut root = Dir::new();

    for entry in entries {
        let components: Vec<&str> = entry.path.split('/').collect();
        if entry.is_folder {
            let dir = root.descend(&components);
            dir.collapsed = entry.collapsed;
        } else {
            let (name, parents) = components
                .split_last()
                .expect("split always yields at least one component");
            let dir = root.descend(parents);
            dir.notes
                .insert((*name).to_string(), (entry.path.clone(), entry.title));
        }
    }

    fn to_node(name: &str, path: &str, dir: &Dir) -> TreeNode {
        let mut children = Vec::with_capacity(dir.subdirs.len() + dir.notes.len());

        // Folders before notes, each alphabetical — the convention every file
        // browser uses, and the one Obsidian's sidebar follows.
        for (child_name, child) in &dir.subdirs {
            children.push(to_node(child_name, &paths::join(path, child_name), child));
        }
        for (file_name, (note_path, title)) in &dir.notes {
            children.push(TreeNode::Note {
                name: file_name.clone(),
                path: note_path.clone(),
                title: title.clone(),
            });
        }

        TreeNode::Folder {
            name: name.to_string(),
            path: path.to_string(),
            collapsed: dir.collapsed,
            children,
        }
    }

    to_node("", "", &root)
}

pub async fn create_folder(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<CreateFolderRequest>,
) -> AppResult<Response> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_folder(&body.path)?;
    if path.rel().is_empty() {
        return Err(AppError::bad_request("the vault root already exists"));
    }

    store::create_folder(&path).await?;

    sqlx::query(
        "INSERT INTO folders (user_id, rel_path) VALUES ($1, $2)
         ON CONFLICT (user_id, rel_path) DO NOTHING",
    )
    .bind(user.id)
    .bind(path.rel())
    .execute(&state.pool)
    .await?;

    tracing::info!(user = %user.username, path = %path.rel(), "created folder");
    Ok(web::no_content())
}

/// Moves a folder and everything under it.
///
/// Every note in the subtree gets a new path, so every link written as a full
/// path has to follow. Links written as a bare filename need no change, which is
/// why the rewrite is per-note rather than a blanket string replacement — see
/// `index::rewrite_link_targets`, which preserves whichever style each author
/// used.
pub async fn move_folder(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<MoveRequest>,
) -> AppResult<Json<MoveResponse>> {
    let vault = state.vault_for(&user)?;
    let from = vault.resolve_folder(&body.from)?;
    let to = vault.resolve_folder(&body.to)?;

    if from.rel().is_empty() {
        return Err(AppError::bad_request("the vault root cannot be moved"));
    }
    if from.rel() == to.rel() {
        return Ok(Json(MoveResponse {
            to: to.rel().to_string(),
            links_rewritten: 0,
        }));
    }
    if !store::is_dir(&from).await {
        return Err(AppError::bad_request("that path is not a folder"));
    }

    // Captured before the move, while the old paths still exist in the index.
    let affected = index::notes_under(&state.pool, user.id, from.rel()).await?;

    store::move_entry(&from, &to).await?;

    let mut links_rewritten = 0usize;
    for old_path in &affected {
        let Some(new_path) = paths::rebase(old_path, from.rel(), to.rel()) else {
            continue;
        };
        index::rename_note_row(&state.pool, user.id, old_path, &new_path).await?;
        links_rewritten +=
            index::rewrite_links_after_move(&state.pool, &user, &vault, old_path, &new_path).await?;
    }

    rebase_folder_rows(&state, &user, from.rel(), to.rel()).await?;

    tracing::info!(
        user = %user.username,
        from = %from.rel(),
        to = %to.rel(),
        notes = affected.len(),
        links_rewritten,
        "moved folder"
    );
    Ok(Json(MoveResponse {
        to: to.rel().to_string(),
        links_rewritten,
    }))
}

/// Rewrites the `folders` rows for a moved subtree, preserving collapse state.
async fn rebase_folder_rows(
    state: &AppState,
    user: &crate::db::User,
    from: &str,
    to: &str,
) -> AppResult<()> {
    let rows = sqlx::query(
        "SELECT rel_path, collapsed FROM folders
         WHERE user_id = $1 AND (rel_path = $2 OR rel_path LIKE $3)",
    )
    .bind(user.id)
    .bind(from)
    .bind(format!("{from}/%"))
    .fetch_all(&state.pool)
    .await?;

    let mut new_paths = Vec::with_capacity(rows.len());
    let mut collapsed = Vec::with_capacity(rows.len());
    for row in &rows {
        let old: String = row.try_get("rel_path")?;
        if let Some(new) = paths::rebase(&old, from, to) {
            new_paths.push(new);
            collapsed.push(row.try_get::<bool, _>("collapsed")?);
        }
    }

    sqlx::query("DELETE FROM folders WHERE user_id = $1 AND (rel_path = $2 OR rel_path LIKE $3)")
        .bind(user.id)
        .bind(from)
        .bind(format!("{from}/%"))
        .execute(&state.pool)
        .await?;

    sqlx::query(
        "INSERT INTO folders (user_id, rel_path, collapsed)
         SELECT $1, f.path, f.collapsed
         FROM unnest($2::text[], $3::bool[]) AS f(path, collapsed)
         ON CONFLICT (user_id, rel_path) DO UPDATE SET collapsed = EXCLUDED.collapsed",
    )
    .bind(user.id)
    .bind(&new_paths)
    .bind(&collapsed)
    .execute(&state.pool)
    .await?;

    Ok(())
}

pub async fn delete_folder(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
) -> AppResult<Response> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve_folder(&rel_path)?;

    if path.rel().is_empty() {
        return Err(AppError::bad_request("the vault root cannot be deleted"));
    }
    if !store::is_dir(&path).await {
        return Err(AppError::bad_request("that path is not a folder"));
    }

    let affected = index::notes_under(&state.pool, user.id, path.rel()).await?;
    let trash_path = store::move_to_trash(&vault, &path).await?;

    for note_path in &affected {
        index::remove_note(&state.pool, user.id, note_path).await?;
    }

    sqlx::query("DELETE FROM folders WHERE user_id = $1 AND (rel_path = $2 OR rel_path LIKE $3)")
        .bind(user.id)
        .bind(path.rel())
        .bind(format!("{}/%", path.rel()))
        .execute(&state.pool)
        .await?;

    tracing::info!(
        user = %user.username,
        path = %path.rel(),
        notes = affected.len(),
        trash = %trash_path,
        "deleted folder"
    );
    Ok(web::no_content())
}

/// Records whether a folder is collapsed in the sidebar.
///
/// The one piece of state in the database that is not derivable from disk:
/// "collapsed" is a property of this user's view, not of the filesystem.
pub async fn set_folder_state(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<FolderStateRequest>,
) -> AppResult<Response> {
    paths::validate_folder_path(&body.path)?;

    sqlx::query(
        "INSERT INTO folders (user_id, rel_path, collapsed) VALUES ($1, $2, $3)
         ON CONFLICT (user_id, rel_path) DO UPDATE SET collapsed = EXCLUDED.collapsed",
    )
    .bind(user.id)
    .bind(&body.path)
    .bind(body.collapsed)
    .execute(&state.pool)
    .await?;

    Ok(web::no_content())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str) -> Entry {
        Entry {
            path: path.into(),
            title: paths::stem(path).into(),
            is_folder: false,
            collapsed: false,
        }
    }

    fn folder(path: &str, collapsed: bool) -> Entry {
        Entry {
            path: path.into(),
            title: String::new(),
            is_folder: true,
            collapsed,
        }
    }

    fn names(node: &TreeNode) -> Vec<String> {
        match node {
            TreeNode::Folder { children, .. } => {
                children.iter().map(|c| c.name().to_string()).collect()
            }
            TreeNode::Note { .. } => Vec::new(),
        }
    }

    fn child<'a>(node: &'a TreeNode, name: &str) -> &'a TreeNode {
        match node {
            TreeNode::Folder { children, .. } => children
                .iter()
                .find(|c| c.name() == name)
                .unwrap_or_else(|| panic!("no child named {name}")),
            TreeNode::Note { .. } => panic!("a note has no children"),
        }
    }

    #[test]
    fn nests_notes_under_their_folders() {
        let tree = build_tree(vec![
            note("Projects/Kitchen Reno.md"),
            note("Projects/Sub/Deep.md"),
            note("Root.md"),
        ]);

        assert_eq!(names(&tree), vec!["Projects", "Root.md"]);
        let projects = child(&tree, "Projects");
        assert_eq!(names(projects), vec!["Sub", "Kitchen Reno.md"]);
        assert_eq!(names(child(projects, "Sub")), vec!["Deep.md"]);
    }

    /// Folders come before notes, and each group is alphabetical, so the sidebar
    /// does not reshuffle itself between requests.
    #[test]
    fn orders_folders_before_notes_and_sorts_each() {
        let tree = build_tree(vec![
            note("zebra.md"),
            note("apple.md"),
            folder("Zoo", false),
            folder("Alpha", false),
        ]);
        assert_eq!(names(&tree), vec!["Alpha", "Zoo", "apple.md", "zebra.md"]);
    }

    #[test]
    fn keeps_empty_folders() {
        let tree = build_tree(vec![folder("Empty", false), folder("Empty/Nested", false)]);
        assert_eq!(names(&tree), vec!["Empty"]);
        assert_eq!(names(child(&tree, "Empty")), vec!["Nested"]);
    }

    #[test]
    fn carries_collapse_state() {
        let tree = build_tree(vec![
            folder("Collapsed", true),
            folder("Open", false),
            note("Collapsed/A.md"),
        ]);
        match child(&tree, "Collapsed") {
            TreeNode::Folder { collapsed, .. } => assert!(*collapsed),
            _ => panic!("expected a folder"),
        }
        match child(&tree, "Open") {
            TreeNode::Folder { collapsed, .. } => assert!(!*collapsed),
            _ => panic!("expected a folder"),
        }
    }

    /// The structure has to come out right from note paths alone, so that a tree
    /// is still correct if the `folders` rows lag behind the filesystem.
    #[test]
    fn materialises_folders_from_note_paths_alone() {
        let tree = build_tree(vec![note("A/B/C/deep.md")]);
        let a = child(&tree, "A");
        let b = child(a, "B");
        let c = child(b, "C");
        assert_eq!(names(c), vec!["deep.md"]);
        assert_eq!(c.path(), "A/B/C");
    }

    #[test]
    fn an_empty_vault_yields_an_empty_root() {
        let tree = build_tree(vec![]);
        assert_eq!(tree.path(), "");
        assert!(tree.is_folder());
        assert!(names(&tree).is_empty());
    }
}
