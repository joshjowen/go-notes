//! Application state, shared through Leptos context.

use std::collections::HashSet;

use go_notes_shared::{Backlink, Me, TreeNode};
use leptos::prelude::*;

use crate::editor::EditorMode;

/// A note open in a tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub path: String,
    pub title: String,
    /// The hash the server last confirmed, used as the `If-Match` token.
    pub content_hash: String,
    /// The user has typed since the last successful save.
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeftPanel {
    Files,
    Search,
    Tags,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RightPanel {
    Backlinks,
    Outline,
    Hidden,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MainView {
    Editor,
    Graph,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Palette {
    /// Ctrl+P — jump to a note by name.
    QuickSwitch,
    /// Ctrl+Shift+P — run a command.
    Commands,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    pub fn as_str(self) -> &'static str {
        match self {
            Theme::Dark => "dark",
            Theme::Light => "light",
        }
    }

    pub fn toggled(self) -> Theme {
        match self {
            Theme::Dark => Theme::Light,
            Theme::Light => Theme::Dark,
        }
    }
}

/// A transient message shown in the corner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    /// Incremented per toast so an identical message still re-triggers the timer.
    pub seq: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

/// A conflict awaiting the user's decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub theirs: String,
    pub their_hash: String,
    pub mine: String,
}

#[derive(Clone, Copy)]
pub struct AppState {
    pub me: RwSignal<Option<Me>>,
    pub tree: RwSignal<Option<TreeNode>>,
    pub tabs: RwSignal<Vec<Tab>>,
    pub active: RwSignal<Option<usize>>,
    pub backlinks: RwSignal<Vec<Backlink>>,
    /// The markdown the editor currently holds. Kept here so the outline pane
    /// can be derived from it without reaching into the editor.
    pub active_markdown: RwSignal<String>,
    pub left_panel: RwSignal<LeftPanel>,
    pub right_panel: RwSignal<RightPanel>,
    pub main_view: RwSignal<MainView>,
    pub palette: RwSignal<Option<Palette>>,
    pub theme: RwSignal<Theme>,
    pub editor_mode: RwSignal<EditorMode>,
    pub toast: RwSignal<Option<Toast>>,
    pub conflict: RwSignal<Option<Conflict>>,
    /// Bumped whenever the tree needs refetching.
    pub tree_epoch: RwSignal<u32>,
    /// Bumped whenever the graph needs refetching.
    pub graph_epoch: RwSignal<u32>,
    /// Bumped by Ctrl+S. The editor pane owns the actual save, so this is how a
    /// keystroke handled by the shell reaches it — a callback in context would
    /// have to be Send, which a closure over the editor handle cannot be.
    pub save_requested: RwSignal<u32>,
    toast_seq: RwSignal<u32>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> AppState {
        AppState {
            me: RwSignal::new(None),
            tree: RwSignal::new(None),
            tabs: RwSignal::new(Vec::new()),
            active: RwSignal::new(None),
            backlinks: RwSignal::new(Vec::new()),
            active_markdown: RwSignal::new(String::new()),
            left_panel: RwSignal::new(LeftPanel::Files),
            right_panel: RwSignal::new(RightPanel::Backlinks),
            main_view: RwSignal::new(MainView::Editor),
            palette: RwSignal::new(None),
            theme: RwSignal::new(Theme::Dark),
            editor_mode: RwSignal::new(EditorMode::Wysiwyg),
            toast: RwSignal::new(None),
            conflict: RwSignal::new(None),
            tree_epoch: RwSignal::new(0),
            graph_epoch: RwSignal::new(0),
            save_requested: RwSignal::new(0),
            toast_seq: RwSignal::new(0),
        }
    }

    pub fn notify(&self, message: impl Into<String>) {
        self.push_toast(message.into(), ToastKind::Info);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.push_toast(message.into(), ToastKind::Error);
    }

    fn push_toast(&self, message: String, kind: ToastKind) {
        let seq = self.toast_seq.get_untracked().wrapping_add(1);
        self.toast_seq.set(seq);
        self.toast.set(Some(Toast { message, kind, seq }));
    }

    pub fn request_save(&self) {
        self.save_requested
            .update(|count| *count = count.wrapping_add(1));
    }

    pub fn refresh_tree(&self) {
        self.tree_epoch.update(|epoch| *epoch = epoch.wrapping_add(1));
    }

    pub fn refresh_graph(&self) {
        self.graph_epoch
            .update(|epoch| *epoch = epoch.wrapping_add(1));
    }

    /// Everything that changed on the server; refetch both views.
    pub fn refresh_all(&self) {
        self.refresh_tree();
        self.refresh_graph();
    }

    pub fn active_tab(&self) -> Option<Tab> {
        let index = self.active.get()?;
        self.tabs.get().get(index).cloned()
    }

    pub fn active_path(&self) -> Option<String> {
        self.active_tab().map(|tab| tab.path)
    }

    /// Opens a note, focusing the existing tab if it is already open.
    pub fn open_tab(&self, path: String, title: String) {
        let existing = self
            .tabs
            .get_untracked()
            .iter()
            .position(|tab| tab.path == path);

        match existing {
            Some(index) => self.active.set(Some(index)),
            None => {
                self.tabs.update(|tabs| {
                    tabs.push(Tab {
                        path,
                        title,
                        content_hash: String::new(),
                        dirty: false,
                    })
                });
                let last = self.tabs.with_untracked(|tabs| tabs.len().saturating_sub(1));
                self.active.set(Some(last));
            }
        }
        self.main_view.set(MainView::Editor);
    }

    pub fn close_tab(&self, index: usize) {
        self.tabs.update(|tabs| {
            if index < tabs.len() {
                tabs.remove(index);
            }
        });

        let remaining = self.tabs.with_untracked(|tabs| tabs.len());
        self.active.update(|active| {
            *active = match *active {
                _ if remaining == 0 => None,
                // Closing a tab before the active one shifts it left.
                Some(current) if current > index => Some(current - 1),
                // Closing the active tab focuses its neighbour.
                Some(current) if current == index => Some(current.min(remaining - 1)),
                other => other,
            };
        });
    }

    /// Renames a tab in place after the note it holds has been moved.
    pub fn rename_tab(&self, from: &str, to: &str) {
        self.tabs.update(|tabs| {
            for tab in tabs.iter_mut() {
                if tab.path == from {
                    tab.path = to.to_string();
                    tab.title = title_of(to);
                } else if let Some(rebased) = go_notes_shared::paths::rebase(&tab.path, from, to) {
                    // A folder move carries every open note inside it.
                    tab.title = title_of(&rebased);
                    tab.path = rebased;
                }
            }
        });
    }

    pub fn close_tabs_under(&self, path: &str) {
        self.tabs.update(|tabs| {
            tabs.retain(|tab| tab.path != path && !go_notes_shared::paths::is_within(&tab.path, path))
        });
        let remaining = self.tabs.with_untracked(|tabs| tabs.len());
        self.active.set(if remaining == 0 {
            None
        } else {
            Some(remaining - 1)
        });
    }

    /// Marks a tab dirty, doing nothing if it already is.
    ///
    /// The early return is not just an optimisation. `RwSignal::update` notifies
    /// every subscriber whether or not the value actually changed, and this is
    /// called on every keystroke — so an unconditional write would re-run
    /// anything watching the tab list several times a second.
    pub fn mark_dirty(&self, path: &str, dirty: bool) {
        let unchanged = self.tabs.with_untracked(|tabs| {
            tabs.iter()
                .any(|tab| tab.path == path && tab.dirty == dirty)
        });
        if unchanged {
            return;
        }
        self.tabs.update(|tabs| {
            for tab in tabs.iter_mut() {
                if tab.path == path {
                    tab.dirty = dirty;
                }
            }
        });
    }

    pub fn set_hash(&self, path: &str, hash: String) {
        let unchanged = self.tabs.with_untracked(|tabs| {
            tabs.iter()
                .any(|tab| tab.path == path && tab.content_hash == hash)
        });
        if unchanged {
            return;
        }
        self.tabs.update(|tabs| {
            for tab in tabs.iter_mut() {
                if tab.path == path {
                    tab.content_hash = hash.clone();
                }
            }
        });
    }

    pub fn hash_for(&self, path: &str) -> String {
        self.tabs
            .with_untracked(|tabs| {
                tabs.iter()
                    .find(|tab| tab.path == path)
                    .map(|tab| tab.content_hash.clone())
            })
            .unwrap_or_default()
    }

    /// Every note path in the vault, for styling unresolved links.
    pub fn known_targets(&self) -> Vec<String> {
        let mut targets = HashSet::new();
        if let Some(tree) = self.tree.get() {
            collect_paths(&tree, &mut targets);
        }
        targets.into_iter().collect()
    }
}

fn collect_paths(node: &TreeNode, into: &mut HashSet<String>) {
    match node {
        TreeNode::Folder { children, .. } => {
            for child in children {
                collect_paths(child, into);
            }
        }
        TreeNode::Note { path, title, .. } => {
            // Both forms a link can name a note by: its full path without the
            // extension, and its bare filename.
            let without_extension = path.strip_suffix(".md").unwrap_or(path);
            into.insert(without_extension.to_string());
            into.insert(go_notes_shared::paths::stem(path).to_string());
            into.insert(title.clone());
        }
    }
}

pub fn title_of(path: &str) -> String {
    go_notes_shared::paths::stem(path).to_string()
}

/// Reads the state out of context. Panics only if the provider is missing, which
/// would be a programming error rather than a runtime condition.
pub fn use_app() -> AppState {
    use_context::<AppState>().expect("AppState was not provided")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str) -> TreeNode {
        TreeNode::Note {
            name: go_notes_shared::paths::basename(path).to_string(),
            path: path.to_string(),
            title: title_of(path),
        }
    }

    #[test]
    fn collects_every_form_a_link_can_name_a_note_by() {
        let tree = TreeNode::Folder {
            name: String::new(),
            path: String::new(),
            collapsed: false,
            children: vec![TreeNode::Folder {
                name: "Projects".into(),
                path: "Projects".into(),
                collapsed: false,
                children: vec![note("Projects/Kitchen Reno.md")],
            }],
        };

        let mut found = HashSet::new();
        collect_paths(&tree, &mut found);

        // `[[Kitchen Reno]]` and `[[Projects/Kitchen Reno]]` must both count as
        // resolved, or the editor would draw a valid link as broken.
        assert!(found.contains("Kitchen Reno"));
        assert!(found.contains("Projects/Kitchen Reno"));
    }

    #[test]
    fn titles_come_from_the_filename() {
        assert_eq!(title_of("Projects/Kitchen Reno.md"), "Kitchen Reno");
        assert_eq!(title_of("Note.md"), "Note");
    }
}
