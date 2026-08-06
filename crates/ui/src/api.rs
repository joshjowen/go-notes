//! The typed client for the server's REST API.
//!
//! Every request goes through here, so the places that need care — sending the
//! `If-Match` header on saves, distinguishing a 409 conflict from an ordinary
//! failure, redirecting to the login screen on a 401 — are handled once rather
//! than at each call site.

use gloo_net::http::{Request, Response};
use go_notes_shared::{
    ApiError, AttachmentResponse, AuthInfo, ConflictBody, CreateFolderRequest, CreateNoteRequest,
    FolderStateRequest, GraphResponse, LoginRequest, Me, MoveRequest, MoveResponse, NoteResponse,
    QuickSwitchItem, SaveNoteRequest, SaveNoteResponse, SearchResponse, TagCount, TreeNode,
    IF_MATCH_HEADER,
};
use serde::de::DeserializeOwned;

/// Everything that can go wrong with a request, in the shapes callers act on.
#[derive(Debug, Clone)]
pub enum ApiFailure {
    /// The request never reached the server: no network, the server is down, or
    /// a proxy in between is not answering.
    ///
    /// Distinct from every other variant on purpose. This is the one failure
    /// that means "try again later, and work locally in the meantime"; the rest
    /// mean the server considered the request and declined it, and retrying
    /// unchanged would only fail again.
    Offline(String),
    /// The session expired or was never established.
    Unauthenticated,
    /// A save lost its optimistic-concurrency check. Carries the current file so
    /// the editor can offer a choice without another round trip.
    Conflict(ConflictBody),
    /// Something is already at that path.
    AlreadyExists(String),
    /// Nothing is at that path.
    NotFound,
    /// Anything else, with a message safe to show the user.
    Message(String),
}

impl ApiFailure {
    pub fn user_message(&self) -> String {
        match self {
            ApiFailure::Offline(_) => "The server could not be reached.".into(),
            ApiFailure::Unauthenticated => "Your session has expired. Please sign in again.".into(),
            ApiFailure::Conflict(_) => "This note changed on disk since you opened it.".into(),
            ApiFailure::AlreadyExists(message) => message.clone(),
            ApiFailure::NotFound => "That is no longer there.".into(),
            ApiFailure::Message(message) => message.clone(),
        }
    }

    /// True when the request never got an answer, so the same request is worth
    /// queueing rather than reporting as an error.
    pub fn is_offline(&self) -> bool {
        matches!(self, ApiFailure::Offline(_))
    }
}

pub type ApiResult<T> = Result<T, ApiFailure>;

fn network_error(err: gloo_net::Error) -> ApiFailure {
    ApiFailure::Offline(format!("could not reach the server: {err}"))
}

/// A request body that could not be built. Distinct from [`network_error`]
/// because it is a bug rather than an outage, and queueing it for later would
/// only fail again in exactly the same way.
fn encode_error(err: gloo_net::Error) -> ApiFailure {
    ApiFailure::Message(format!("Could not build the request: {err}"))
}

/// Turns a response into either the decoded body or a typed failure.
async fn decode<T: DeserializeOwned>(response: Response) -> ApiResult<T> {
    if response.ok() {
        return response
            .json::<T>()
            .await
            .map_err(|err| ApiFailure::Message(format!("Unexpected response: {err}")));
    }
    Err(failure_from(response).await)
}

async fn expect_success(response: Response) -> ApiResult<()> {
    if response.ok() {
        return Ok(());
    }
    Err(failure_from(response).await)
}

async fn failure_from(response: Response) -> ApiFailure {
    let status = response.status();

    if status == 401 {
        return ApiFailure::Unauthenticated;
    }

    if status == 404 {
        return ApiFailure::NotFound;
    }

    // 409 on a save carries the note as it currently exists on disk. The other
    // 409 — something is already at that path — matters to the sync layer,
    // which replays a queued create against a server that may have grown the
    // same note in the meantime.
    if status == 409 {
        if let Ok(body) = response.json::<ConflictBody>().await {
            if body.code == "conflict" {
                return ApiFailure::Conflict(body);
            }
            return ApiFailure::AlreadyExists(body.message);
        }
        return ApiFailure::Message("That conflicts with something already there.".into());
    }

    match response.json::<ApiError>().await {
        Ok(error) => ApiFailure::Message(error.message),
        Err(_) => ApiFailure::Message(format!("The server returned an error ({status}).")),
    }
}

/// Percent-encodes a vault path for a URL, keeping `/` as a separator.
///
/// Note paths routinely contain spaces, `#` and `?`, all of which would
/// otherwise change what the server sees.
fn encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            segment
                .bytes()
                .map(|byte| match byte {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                        (byte as char).to_string()
                    }
                    other => format!("%{other:02X}"),
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_query(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            b' ' => "+".to_string(),
            other => format!("%{other:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

pub async fn auth_info() -> ApiResult<AuthInfo> {
    let response = Request::get("/api/auth/info")
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn me() -> ApiResult<Me> {
    let response = Request::get("/api/me").send().await.map_err(network_error)?;
    decode(response).await
}

pub async fn login(username: String, password: String) -> ApiResult<Me> {
    let response = Request::post("/api/auth/login")
        .json(&LoginRequest { username, password })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct LogoutResponse {
    pub redirect_to: Option<String>,
}

pub async fn logout() -> ApiResult<LogoutResponse> {
    let response = Request::post("/api/auth/logout")
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

// ---------------------------------------------------------------------------
// Tree and folders
// ---------------------------------------------------------------------------

pub async fn tree() -> ApiResult<TreeNode> {
    let response = Request::get("/api/tree")
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn create_folder(path: String) -> ApiResult<()> {
    let response = Request::post("/api/folders")
        .json(&CreateFolderRequest { path })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    expect_success(response).await
}

pub async fn move_folder(from: String, to: String) -> ApiResult<MoveResponse> {
    let response = Request::post("/api/folders/move")
        .json(&MoveRequest { from, to })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn delete_folder(path: String) -> ApiResult<()> {
    let response = Request::delete(&format!("/api/folders/{}", encode_path(&path)))
        .send()
        .await
        .map_err(network_error)?;
    expect_success(response).await
}

pub async fn set_folder_collapsed(path: String, collapsed: bool) -> ApiResult<()> {
    let response = Request::post("/api/folders/state")
        .json(&FolderStateRequest { path, collapsed })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    expect_success(response).await
}

// ---------------------------------------------------------------------------
// Notes
// ---------------------------------------------------------------------------

pub async fn read_note(path: String) -> ApiResult<NoteResponse> {
    let response = Request::get(&format!("/api/notes/{}", encode_path(&path)))
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

/// Saves a note, guarded by the hash the client last saw.
///
/// The `If-Match` header is what turns a lost update into a visible conflict.
/// Without it, two tabs open on the same note would silently overwrite each
/// other, and so would an edit made over SSH while a tab sat open.
pub async fn save_note(
    path: String,
    markdown: String,
    expected_hash: String,
) -> ApiResult<SaveNoteResponse> {
    let response = Request::put(&format!("/api/notes/{}", encode_path(&path)))
        .header(IF_MATCH_HEADER, &expected_hash)
        .json(&SaveNoteRequest { markdown })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn create_note(path: String, markdown: String) -> ApiResult<SaveNoteResponse> {
    let response = Request::post("/api/notes")
        .json(&CreateNoteRequest { path, markdown })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn delete_note(path: String) -> ApiResult<()> {
    let response = Request::delete(&format!("/api/notes/{}", encode_path(&path)))
        .send()
        .await
        .map_err(network_error)?;
    expect_success(response).await
}

/// Notes linking to this one, for the side panel.
pub async fn backlinks(path: String) -> ApiResult<Vec<go_notes_shared::Backlink>> {
    let response = Request::get(&format!("/api/backlinks/{}", encode_path(&path)))
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn move_note(from: String, to: String) -> ApiResult<MoveResponse> {
    let response = Request::post("/api/notes/move")
        .json(&MoveRequest { from, to })
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

// ---------------------------------------------------------------------------
// Search, tags, graph
// ---------------------------------------------------------------------------

pub async fn search(query: String) -> ApiResult<SearchResponse> {
    let response = Request::get(&format!("/api/search?q={}", encode_query(&query)))
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn quickswitch(query: String) -> ApiResult<Vec<QuickSwitchItem>> {
    let response = Request::get(&format!("/api/quickswitch?q={}", encode_query(&query)))
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn tags() -> ApiResult<Vec<TagCount>> {
    let response = Request::get("/api/tags")
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn notes_with_tag(tag: String) -> ApiResult<Vec<QuickSwitchItem>> {
    let response = Request::get(&format!("/api/tagged?tag={}", encode_query(&tag)))
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

pub async fn graph(
    scope: &str,
    path: Option<&str>,
    depth: u32,
    semantic: bool,
) -> ApiResult<GraphResponse> {
    // Only sent when asked for: the server leaves the suggestion query out
    // entirely without it, so the default payload is what it always was.
    let suggestions = if semantic { "&semantic=true" } else { "" };
    let url = match path {
        Some(path) if scope == "local" => format!(
            "/api/graph?scope=local&depth={depth}{suggestions}&path={}",
            encode_query(path)
        ),
        _ => format!("/api/graph?scope=all{suggestions}"),
    };
    let response = Request::get(&url).send().await.map_err(network_error)?;
    decode(response).await
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// Uploads a file, returning where to embed it.
pub async fn upload_attachment(file: web_sys::File) -> ApiResult<AttachmentResponse> {
    let form = web_sys::FormData::new()
        .map_err(|_| ApiFailure::Message("Could not prepare the upload.".into()))?;
    form.append_with_blob_and_filename("file", &file, &file.name())
        .map_err(|_| ApiFailure::Message("Could not attach the file.".into()))?;

    let response = Request::post("/api/attachments")
        .body(form)
        .map_err(encode_error)?
        .send()
        .await
        .map_err(network_error)?;
    decode(response).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Note paths contain spaces and punctuation constantly; getting this wrong
    /// means the server sees a different path than the one the user clicked.
    #[test]
    fn encodes_path_segments_but_keeps_separators() {
        assert_eq!(encode_path("Projects/Kitchen Reno.md"), "Projects/Kitchen%20Reno.md");
        assert_eq!(encode_path("a/b.md"), "a/b.md");
        assert_eq!(encode_path("Q&A #1.md"), "Q%26A%20%231.md");
        assert_eq!(encode_path("caf\u{e9}.md"), "caf%C3%A9.md");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query("hello world"), "hello+world");
        assert_eq!(encode_query("a&b=c"), "a%26b%3Dc");
        assert_eq!(encode_query("#tag"), "%23tag");
    }
}
