//! Types shared by the `go-notes` server and its Leptos frontend.
//!
//! This crate is the API contract. It must stay free of any dependency that
//! cannot compile to `wasm32-unknown-unknown`, so no tokio, no sqlx, no fs.

use serde::{Deserialize, Serialize};

pub mod links;
pub mod paths;

/// Header used for optimistic-concurrency checks when saving a note.
///
/// The client echoes back the `content_hash` it last read; the server refuses
/// the write if the file on disk has moved on since then.
pub const IF_MATCH_HEADER: &str = "if-match";

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Me {
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    /// `"local"` or `"oidc"` — the frontend uses this to decide whether the
    /// logout button should also bounce through the identity provider.
    pub auth_provider: String,
    /// Whether this server has an embeddings model configured.
    ///
    /// Rides along with the identity because that is the one thing the app
    /// fetches for a signed-in user, and a whole endpoint for one boolean would
    /// be a round trip on every load. It is here rather than on `AuthInfo`
    /// because that is only ever read by the login screen.
    #[serde(default)]
    pub semantic_links: bool,
}

/// What the login screen should offer, fetched before the user authenticates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AuthInfo {
    /// True when the server accepts username/password against its local file.
    pub local_enabled: bool,
    /// Present when an OIDC provider is configured; the label to put on the button.
    pub oidc_button: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

// ---------------------------------------------------------------------------
// Tree
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TreeNode {
    Folder {
        /// Display name (last path component). Empty for the vault root.
        name: String,
        /// Vault-relative path, `/`-separated. Empty string for the vault root.
        path: String,
        collapsed: bool,
        children: Vec<TreeNode>,
    },
    Note {
        name: String,
        path: String,
        title: String,
    },
}

impl TreeNode {
    pub fn path(&self) -> &str {
        match self {
            TreeNode::Folder { path, .. } | TreeNode::Note { path, .. } => path,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            TreeNode::Folder { name, .. } | TreeNode::Note { name, .. } => name,
        }
    }

    pub fn is_folder(&self) -> bool {
        matches!(self, TreeNode::Folder { .. })
    }
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteMeta {
    pub path: String,
    pub title: String,
    /// blake3 of the exact bytes on disk; used as the If-Match token.
    pub content_hash: String,
    pub modified: chrono::DateTime<chrono::Utc>,
    pub size_bytes: i64,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NoteResponse {
    pub meta: NoteMeta,
    /// The full file contents, frontmatter included, exactly as stored.
    pub markdown: String,
    pub backlinks: Vec<Backlink>,
    pub outgoing: Vec<OutgoingLink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Backlink {
    /// Path of the note that links here.
    pub path: String,
    pub title: String,
    /// A short excerpt around the link, for the backlinks pane.
    pub context: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OutgoingLink {
    /// Literal link target as written in the note.
    pub target_raw: String,
    /// Resolved note path, or `None` when the link is broken.
    pub resolved_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveNoteRequest {
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaveNoteResponse {
    pub meta: NoteMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateNoteRequest {
    /// Vault-relative path including the `.md` extension.
    pub path: String,
    #[serde(default)]
    pub markdown: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveRequest {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveResponse {
    pub to: String,
    /// How many other notes had their `[[wikilinks]]` rewritten to follow the move.
    pub links_rewritten: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateFolderRequest {
    pub path: String,
}

/// Sidebar collapse state is per-user UI state, so it lives in Postgres rather
/// than on disk — a collapsed folder is not a property of the filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FolderStateRequest {
    pub path: String,
    pub collapsed: bool,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    /// Highlighted excerpt; `«` and `»` delimit matched terms.
    pub snippet: String,
    pub rank: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
}

/// Delimiters used by the server when marking matched terms in a snippet.
/// Chosen so they cannot collide with anything the highlighter might emit as
/// HTML — the frontend splits on them and renders `<mark>` itself.
pub const SNIPPET_OPEN: char = '\u{00ab}'; // «
pub const SNIPPET_CLOSE: char = '\u{00bb}'; // »

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QuickSwitchItem {
    pub path: String,
    pub title: String,
    /// False when this entry is an offer to create a note that does not exist yet.
    pub exists: bool,
}

// ---------------------------------------------------------------------------
// Graph
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphNode {
    pub id: u32,
    pub path: String,
    pub title: String,
    /// Folder the note lives in, used for colouring. Empty for the vault root.
    pub folder: String,
    pub tags: Vec<String>,
    /// Total number of links in or out; drives node radius.
    pub degree: u32,
    /// True for a link target that has no file behind it yet.
    pub unresolved: bool,
}

/// What kind of relationship an edge represents.
///
/// The distinction is worth drawing because the three are not equally certain.
/// A `Link` is a fact about the file; a `Typed` link is that plus a word the
/// author chose for it; a `Semantic` edge is a machine's guess, and the graph
/// draws it as one.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// An ordinary `[[wikilink]]`, embed, or inline markdown link.
    #[default]
    Link,
    /// A `[[relation::Note]]` link, carrying the author's own word for it.
    Typed,
    /// Not a link at all: two passages a model found similar. A suggestion, and
    /// drawn as one.
    Semantic,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphEdge {
    pub source: u32,
    pub target: u32,
    /// Absent in older payloads, which only ever meant `Link`.
    #[serde(default)]
    pub kind: EdgeKind,
    /// The author's word for the relationship. Only ever set on `Typed`.
    #[serde(default)]
    pub relation: Option<String>,
    /// How strongly the two are tied: 1.0 for a link somebody wrote, the
    /// similarity score for a `Semantic` edge. Drives both how faintly the edge
    /// is drawn and how hard its spring pulls.
    #[serde(default = "unit_weight")]
    pub weight: f32,
}

fn unit_weight() -> f32 {
    1.0
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GraphResponse {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

// ---------------------------------------------------------------------------
// Tags and attachments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagCount {
    pub name: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentResponse {
    /// Vault-relative path, e.g. `attachments/2026/diagram-a1b2c3.png`.
    pub path: String,
    /// URL the editor should embed, e.g. `/api/files/attachments/2026/...`.
    pub url: String,
    pub mime: String,
    pub size_bytes: i64,
    /// True for types the editor should render inline as an image.
    pub is_image: bool,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Uniform error body for every non-2xx API response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApiError {
    /// Stable machine-readable code, e.g. `conflict`, `not_found`, `invalid_path`.
    pub code: String,
    pub message: String,
}

/// Returned with 409 when a save loses the If-Match check, so the client can
/// offer a three-way choice without a second round trip.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictBody {
    pub code: String,
    pub message: String,
    /// The note as it currently exists on disk.
    pub current_markdown: String,
    pub current_hash: String,
}
