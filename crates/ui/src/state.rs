//! Application state, shared through Leptos context.

use std::collections::HashSet;

use go_notes_shared::{Backlink, Me, TreeNode};
use leptos::prelude::*;

use crate::editor::EditorMode;
use crate::theme::{ThemeColors, ThemeId};

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
    pub origin: ConflictOrigin,
}

/// Where a conflict came from, which decides what resolving it has to tidy up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictOrigin {
    /// A save from the open editor lost its `If-Match` check.
    Live,
    /// A change made offline could not be replayed as it stood. Resolving it
    /// also settles the queued operation identified here.
    Sync { op_id: u64 },
}

/// What the sync engine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPhase {
    /// Nothing to do, or nothing being done.
    Idle,
    /// Replaying the outbox.
    Syncing,
    /// Stopped and waiting for a person: a conflict to resolve, or a session to
    /// sign back into. Queued work is intact.
    Blocked,
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
    pub theme_id: RwSignal<ThemeId>,
    pub custom_colors: RwSignal<ThemeColors>,
    pub custom_css: RwSignal<String>,
    pub theme_dialog_open: RwSignal<bool>,
    pub editor_mode: RwSignal<EditorMode>,
    pub toast: RwSignal<Option<Toast>>,
    /// Conflicts waiting for a decision; the dialog shows the first of them.
    pub conflicts: RwSignal<Vec<Conflict>>,
    /// False when the server could not be reached: the app is running on what
    /// this device holds, and every change is being queued.
    pub online: RwSignal<bool>,
    pub sync: RwSignal<SyncPhase>,
    /// Changes made here that the server has not accepted yet.
    pub pending: RwSignal<Vec<crate::offline::queue::QueuedOp>>,
    /// Why syncing is paused, when it is — an expired session, say.
    pub sync_message: RwSignal<Option<String>>,
    /// False when the browser will not give us local storage at all (a private
    /// window, a full disk, a strict privacy setting), which changes what we
    /// can honestly promise about working offline.
    pub offline_storage: RwSignal<bool>,
    /// Bumped when the open note should be re-read from the vault — after a
    /// sync, or when the user chooses the server's version of a conflict.
    pub reload_requested: RwSignal<u32>,
    /// The browser has offered to install the app, and the offer is being held
    /// until the user asks for it.
    pub installable: RwSignal<bool>,
    /// The file tree drawer, which is only a drawer on a narrow screen. On a
    /// wide one the sidebar is always there and this is ignored.
    pub drawer_open: RwSignal<bool>,
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
            // Backlinks are a side panel on a desktop and a whole screen on a
            // phone, so a narrow window starts without one rather than opening
            // on a note squeezed into a third of the width.
            right_panel: RwSignal::new(if is_narrow() {
                RightPanel::Hidden
            } else {
                RightPanel::Backlinks
            }),
            main_view: RwSignal::new(MainView::Editor),
            palette: RwSignal::new(None),
            theme_id: RwSignal::new(ThemeId::DefaultDark),
            custom_colors: RwSignal::new(ThemeColors::default_dark()),
            custom_css: RwSignal::new(String::new()),
            theme_dialog_open: RwSignal::new(false),
            editor_mode: RwSignal::new(EditorMode::Wysiwyg),
            toast: RwSignal::new(None),
            conflicts: RwSignal::new(Vec::new()),
            // Optimistic until a request says otherwise: the browser's own idea
            // of connectivity is only a starting point, and the first call to
            // `/api/me` settles it either way.
            online: RwSignal::new(crate::offline::net::browser_thinks_online()),
            sync: RwSignal::new(SyncPhase::Idle),
            pending: RwSignal::new(Vec::new()),
            sync_message: RwSignal::new(None),
            offline_storage: RwSignal::new(true),
            reload_requested: RwSignal::new(0),
            installable: RwSignal::new(false),
            drawer_open: RwSignal::new(false),
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

    /// Asks the editor pane to re-read the open note from the vault.
    ///
    /// Used after a sync and after "use the server's version", both of which
    /// change the text under a tab that may be on screen.
    pub fn request_reload(&self) {
        self.reload_requested
            .update(|count| *count = count.wrapping_add(1));
    }

    /// Queues a conflict for the resolution dialog.
    ///
    /// A second conflict on a note already waiting replaces the first: they are
    /// the same disagreement, and showing it twice would ask the user to make
    /// the same decision about stale text.
    pub fn push_conflict(&self, conflict: Conflict) {
        self.conflicts.update(|conflicts| {
            conflicts.retain(|existing| existing.path != conflict.path);
            conflicts.push(conflict);
        });
    }

    pub fn clear_conflict(&self, path: &str) {
        self.conflicts
            .update(|conflicts| conflicts.retain(|conflict| conflict.path != path));
    }

    /// True when the app is running on what this device holds, with the server
    /// unreachable.
    pub fn local_only(&self) -> bool {
        !self.online.get()
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
        // Opening a note is the end of what the drawer is for. Harmless on a
        // wide screen, where the sidebar is not a drawer at all.
        self.drawer_open.set(false);
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

    pub fn close_tab_with_path(&self, path: &str) {
        let index = self
            .tabs
            .get_untracked()
            .iter()
            .position(|tab| tab.path == path);
        if let Some(index) = index {
            self.close_tab(index);
        }
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

/// Whether the window is narrow enough that the sidebar has to become a drawer.
///
/// The same breakpoint as the stylesheet's. Read once at startup, for the
/// initial panel layout; the CSS handles every later resize on its own.
pub fn is_narrow() -> bool {
    web_sys::window()
        .and_then(|window| window.inner_width().ok())
        .and_then(|width| width.as_f64())
        .is_some_and(|width| width <= 820.0)
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
