//! Local edits to the cached file tree.
//!
//! When the server is unreachable the sidebar still has to react to what the
//! user does: create a note and it appears, rename a folder and its contents
//! follow. These functions apply exactly that change to the cached tree, in the
//! same order the server would have returned — folders before notes, each group
//! sorted by name — so that reconnecting and refetching does not visibly
//! reshuffle the sidebar.
//!
//! This is a projection, not a second source of truth. The moment the server
//! answers again, its tree replaces this one.

use go_notes_shared::{paths, TreeNode};

/// Adds a note, creating any folders on the way to it.
pub fn insert_note(tree: &mut TreeNode, path: &str) {
    let title = paths::stem(path).to_string();
    let name = paths::basename(path).to_string();
    let parent = folder_mut(tree, paths::parent_of(path));

    let TreeNode::Folder { children, .. } = parent else {
        return;
    };
    if children.iter().any(|child| child.path() == path) {
        return;
    }
    children.push(TreeNode::Note {
        name,
        path: path.to_string(),
        title,
    });
    sort_children(children);
}

/// Adds a folder, creating any parents on the way to it.
pub fn insert_folder(tree: &mut TreeNode, path: &str) {
    folder_mut(tree, path);
}

/// Removes a note or folder, and everything under it.
pub fn remove(tree: &mut TreeNode, path: &str) {
    let parent_path = paths::parent_of(path).to_string();
    let Some(TreeNode::Folder { children, .. }) = find_folder_mut(tree, &parent_path) else {
        return;
    };
    children.retain(|child| child.path() != path);
}

/// Moves a note or folder, rewriting the paths of everything inside it.
pub fn rename(tree: &mut TreeNode, from: &str, to: &str) {
    let Some(mut node) = take(tree, from) else {
        return;
    };
    rebase_node(&mut node, from, to);

    let parent = folder_mut(tree, paths::parent_of(to));
    let TreeNode::Folder { children, .. } = parent else {
        return;
    };
    children.retain(|child| child.path() != to);
    children.push(node);
    sort_children(children);
}

/// Records a folder's collapsed state, which is per-user UI state the server
/// normally keeps. Held here too so the sidebar looks the same after a reload
/// with no server to ask.
pub fn set_collapsed(tree: &mut TreeNode, path: &str, collapsed: bool) {
    if let Some(TreeNode::Folder {
        collapsed: current, ..
    }) = find_folder_mut(tree, path)
    {
        *current = collapsed;
    }
}

/// Detaches a node from the tree and returns it.
fn take(tree: &mut TreeNode, path: &str) -> Option<TreeNode> {
    let parent_path = paths::parent_of(path).to_string();
    let TreeNode::Folder { children, .. } = find_folder_mut(tree, &parent_path)? else {
        return None;
    };
    let index = children.iter().position(|child| child.path() == path)?;
    Some(children.remove(index))
}

/// Rewrites a subtree's paths after its root has moved.
fn rebase_node(node: &mut TreeNode, from: &str, to: &str) {
    match node {
        TreeNode::Folder {
            name,
            path,
            children,
            ..
        } => {
            *path = rebased(path, from, to);
            *name = paths::basename(path).to_string();
            for child in children {
                rebase_node(child, from, to);
            }
        }
        TreeNode::Note {
            name, path, title, ..
        } => {
            *path = rebased(path, from, to);
            *name = paths::basename(path).to_string();
            *title = paths::stem(path).to_string();
        }
    }
}

fn rebased(path: &str, from: &str, to: &str) -> String {
    if path == from {
        return to.to_string();
    }
    paths::rebase(path, from, to).unwrap_or_else(|| path.to_string())
}

/// Walks to a folder, creating the ones that do not exist yet.
///
/// Creating on the way is what makes `New note` inside a folder that only
/// exists in a queued operation work: the folder is as real to the sidebar as
/// the note about to go in it.
fn folder_mut<'a>(tree: &'a mut TreeNode, path: &str) -> &'a mut TreeNode {
    let mut current = tree;
    if path.is_empty() {
        return current;
    }

    let mut walked = String::new();
    for component in path.split('/') {
        if !walked.is_empty() {
            walked.push('/');
        }
        walked.push_str(component);

        let TreeNode::Folder { children, .. } = current else {
            return current;
        };
        let existing = children.iter().position(|child| child.path() == walked);
        let index = match existing {
            Some(index) => index,
            None => {
                children.push(TreeNode::Folder {
                    name: component.to_string(),
                    path: walked.clone(),
                    collapsed: false,
                    children: Vec::new(),
                });
                sort_children(children);
                children
                    .iter()
                    .position(|child| child.path() == walked)
                    .expect("the folder was just inserted")
            }
        };
        current = &mut children[index];
    }
    current
}

fn find_folder_mut<'a>(tree: &'a mut TreeNode, path: &str) -> Option<&'a mut TreeNode> {
    if path.is_empty() {
        return Some(tree);
    }
    let TreeNode::Folder { children, .. } = tree else {
        return None;
    };
    for child in children.iter_mut() {
        if child.path() == path {
            return Some(child);
        }
        if child.is_folder() && paths::is_within(path, child.path()) {
            return find_folder_mut(child, path);
        }
    }
    None
}

/// Folders first, then notes, each alphabetical — the order the server builds.
fn sort_children(children: &mut [TreeNode]) {
    children.sort_by(|a, b| {
        b.is_folder()
            .cmp(&a.is_folder())
            .then_with(|| a.name().cmp(b.name()))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> TreeNode {
        TreeNode::Folder {
            name: String::new(),
            path: String::new(),
            collapsed: false,
            children: Vec::new(),
        }
    }

    fn paths_of(tree: &TreeNode) -> Vec<String> {
        let mut out = Vec::new();
        fn walk(node: &TreeNode, out: &mut Vec<String>) {
            if !node.path().is_empty() {
                out.push(node.path().to_string());
            }
            if let TreeNode::Folder { children, .. } = node {
                for child in children {
                    walk(child, out);
                }
            }
        }
        walk(tree, &mut out);
        out
    }

    #[test]
    fn inserting_a_note_creates_the_folders_above_it() {
        let mut tree = root();
        insert_note(&mut tree, "Projects/Kitchen/Reno.md");

        assert_eq!(
            paths_of(&tree),
            vec![
                "Projects".to_string(),
                "Projects/Kitchen".to_string(),
                "Projects/Kitchen/Reno.md".to_string()
            ]
        );
    }

    #[test]
    fn folders_come_before_notes_and_each_group_is_sorted() {
        let mut tree = root();
        insert_note(&mut tree, "zebra.md");
        insert_note(&mut tree, "apple.md");
        insert_folder(&mut tree, "Zoo");
        insert_folder(&mut tree, "Alpha");

        assert_eq!(
            paths_of(&tree),
            vec![
                "Alpha".to_string(),
                "Zoo".to_string(),
                "apple.md".to_string(),
                "zebra.md".to_string()
            ]
        );
    }

    #[test]
    fn inserting_the_same_note_twice_does_nothing() {
        let mut tree = root();
        insert_note(&mut tree, "A.md");
        insert_note(&mut tree, "A.md");
        assert_eq!(paths_of(&tree), vec!["A.md".to_string()]);
    }

    #[test]
    fn removing_a_folder_takes_its_contents_with_it() {
        let mut tree = root();
        insert_note(&mut tree, "Projects/A.md");
        insert_note(&mut tree, "Keep.md");

        remove(&mut tree, "Projects");
        assert_eq!(paths_of(&tree), vec!["Keep.md".to_string()]);
    }

    #[test]
    fn renaming_a_note_moves_it_and_retitles_it() {
        let mut tree = root();
        insert_note(&mut tree, "A.md");

        rename(&mut tree, "A.md", "Projects/B.md");
        assert_eq!(
            paths_of(&tree),
            vec!["Projects".to_string(), "Projects/B.md".to_string()]
        );

        let TreeNode::Folder { children, .. } = &tree else {
            panic!("root is a folder")
        };
        let TreeNode::Folder { children, .. } = &children[0] else {
            panic!("Projects is a folder")
        };
        let TreeNode::Note { title, name, .. } = &children[0] else {
            panic!("expected the note")
        };
        assert_eq!(title, "B");
        assert_eq!(name, "B.md");
    }

    /// The case that matters for the sidebar: dragging a folder somewhere else
    /// has to carry every path underneath it, not just its own.
    #[test]
    fn renaming_a_folder_rewrites_the_paths_inside_it() {
        let mut tree = root();
        insert_note(&mut tree, "Projects/Kitchen/Reno.md");

        rename(&mut tree, "Projects", "Archive/Projects");

        assert_eq!(
            paths_of(&tree),
            vec![
                "Archive".to_string(),
                "Archive/Projects".to_string(),
                "Archive/Projects/Kitchen".to_string(),
                "Archive/Projects/Kitchen/Reno.md".to_string(),
            ]
        );
    }

    #[test]
    fn renaming_something_that_is_not_there_is_a_no_op() {
        let mut tree = root();
        insert_note(&mut tree, "A.md");
        rename(&mut tree, "Missing.md", "Other.md");
        assert_eq!(paths_of(&tree), vec!["A.md".to_string()]);
    }
}
