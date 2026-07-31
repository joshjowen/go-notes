//! Attachments: uploading files into a vault and serving them back.
//!
//! Serving user-uploaded files from the same origin as the application is where
//! a stored-XSS hole usually gets in, so three things happen on the way out and
//! none of them is optional:
//!
//! * the `Content-Type` is whatever the bytes actually are, sniffed on upload,
//!   never what the client claimed;
//! * `X-Content-Type-Options: nosniff` stops the browser from second-guessing;
//! * only formats a browser can render safely are sent `inline` — everything
//!   else, including SVG, is `attachment`.
//!
//! SVG deserves the specific mention: it is an image people reasonably expect to
//! paste into notes, and it is also a document format that can carry script. It
//! is accepted, stored and downloadable, but never rendered inline from this
//! origin.

use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Datelike;
use go_notes_shared::paths::{self, ATTACHMENTS_DIR};
use go_notes_shared::AttachmentResponse;

use crate::auth::session::CurrentUser;
use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::vault::store;

/// Types that may be rendered inline. Everything else downloads.
const INLINE_SAFE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
    "application/pdf",
    "audio/mpeg",
    "audio/ogg",
    "audio/wav",
    "video/mp4",
    "video/webm",
];

/// Types the editor should embed as an image.
const IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
];

pub async fn upload(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    mut multipart: Multipart,
) -> AppResult<Json<AttachmentResponse>> {
    let vault = state.vault_for(&user)?;
    let limit = state.config.uploads.max_bytes;

    let mut filename = None;
    let mut data: Option<Vec<u8>> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|err| AppError::bad_request(format!("malformed upload: {err}")))?
    {
        if field.name() != Some("file") {
            continue;
        }
        filename = field.file_name().map(str::to_string);
        let bytes = field
            .bytes()
            .await
            .map_err(|err| AppError::bad_request(format!("could not read upload: {err}")))?;

        if bytes.len() > limit {
            return Err(AppError::TooLarge(format!(
                "that file is {} MB; the limit is {} MB",
                bytes.len() / (1024 * 1024),
                limit / (1024 * 1024)
            )));
        }
        data = Some(bytes.to_vec());
        break;
    }

    let data = data.ok_or_else(|| AppError::bad_request("no file was included in the upload"))?;
    if data.is_empty() {
        return Err(AppError::bad_request("that file is empty"));
    }

    let original = filename.unwrap_or_else(|| "upload".to_string());
    let extension = extension_of(&original);

    if !state
        .config
        .uploads
        .allowed_extensions
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&extension))
    {
        return Err(AppError::UnsupportedMedia(format!(
            "'{extension}' files are not accepted"
        )));
    }

    // The type is decided by the bytes, not by the name. A file called
    // `photo.png` containing HTML must not be served as an image, and — more to
    // the point — must not be served as HTML either.
    let sniffed = infer::get(&data).map(|kind| kind.mime_type().to_string());
    let declared = mime_guess::from_path(&original)
        .first()
        .map(|mime| mime.essence_str().to_string());

    let mime = match (&sniffed, &declared) {
        (Some(sniffed), _) => sniffed.clone(),
        // `infer` recognises binary formats by magic number and returns nothing
        // for text ones, which is fine: a text file has no dangerous rendering
        // beyond what `nosniff` plus a download disposition already prevents.
        (None, Some(declared)) if declared.starts_with("text/") => declared.clone(),
        (None, Some(declared)) if declared == "image/svg+xml" => declared.clone(),
        (None, _) => "application/octet-stream".to_string(),
    };

    // Grouped by year so a long-lived vault's attachments directory stays
    // navigable from a shell rather than becoming one folder of ten thousand files.
    let year = chrono::Utc::now().year();
    let stem = paths::sanitize_component(paths::stem(&original), "file");
    let digest = &store::hash_bytes(&data)[..8];
    let rel_path = format!("{ATTACHMENTS_DIR}/{year}/{stem}-{digest}.{extension}");

    let path = vault.resolve(&rel_path)?;

    // Content-addressed names mean re-uploading the same image is idempotent
    // rather than accumulating copies.
    if !store::exists(&path).await {
        if let Some(parent) = path.abs().parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        store::write_atomic(path.abs(), &data).await?;
    }

    sqlx::query(
        "INSERT INTO attachments (user_id, rel_path, mime, size_bytes)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id, rel_path)
         DO UPDATE SET mime = EXCLUDED.mime, size_bytes = EXCLUDED.size_bytes",
    )
    .bind(user.id)
    .bind(path.rel())
    .bind(&mime)
    .bind(data.len() as i64)
    .execute(&state.pool)
    .await?;

    tracing::info!(
        user = %user.username,
        path = %path.rel(),
        mime = %mime,
        bytes = data.len(),
        "stored attachment"
    );

    Ok(Json(AttachmentResponse {
        url: format!("/api/files/{}", encode_path(path.rel())),
        is_image: IMAGE_MIMES.contains(&mime.as_str()),
        path: path.rel().to_string(),
        size_bytes: data.len() as i64,
        mime,
    }))
}

/// Serves an attachment out of the requesting user's own vault.
///
/// Ownership is structural rather than checked: the path is resolved against
/// `vault_for(user)`, so there is no string a user can send that names a file in
/// somebody else's vault.
pub async fn serve(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(rel_path): Path<String>,
) -> AppResult<Response> {
    let vault = state.vault_for(&user)?;
    let path = vault.resolve(&rel_path)?;

    let data = tokio::fs::read(path.abs()).await?;

    // Prefer the type recorded at upload time, which was sniffed from content.
    let recorded: Option<String> =
        sqlx::query_scalar("SELECT mime FROM attachments WHERE user_id = $1 AND rel_path = $2")
            .bind(user.id)
            .bind(path.rel())
            .fetch_optional(&state.pool)
            .await?;

    let mime = recorded.unwrap_or_else(|| {
        infer::get(&data)
            .map(|kind| kind.mime_type().to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string())
    });

    let disposition = if INLINE_SAFE_MIMES.contains(&mime.as_str()) {
        format!("inline; filename=\"{}\"", quoted(path.basename()))
    } else {
        format!("attachment; filename=\"{}\"", quoted(path.basename()))
    };

    Ok((
        [
            (header::CONTENT_TYPE, mime),
            (header::CONTENT_DISPOSITION, disposition),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            // Attachments are content-addressed, so a given URL's bytes never
            // change and can be cached indefinitely. Private, because the URL is
            // only meaningful for this user's session.
            (
                header::CACHE_CONTROL,
                "private, max-age=31536000, immutable".to_string(),
            ),
        ],
        Body::from(data),
    )
        .into_response())
}

fn extension_of(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext)
        .filter(|ext| !ext.is_empty() && ext.len() <= 10)
        .unwrap_or("bin")
        .to_lowercase()
}

/// Percent-encodes a vault path for use in a URL, leaving `/` as a separator.
fn encode_path(rel_path: &str) -> String {
    rel_path
        .split('/')
        .map(|segment| {
            percent_encoding::utf8_percent_encode(segment, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Escapes a filename for a quoted-string header parameter.
///
/// Without this, a note called `evil".md` could close the quoted string early
/// and inject header parameters.
fn quoted(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control())
        .map(|c| match c {
            '"' | '\\' => '_',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_extensions_defensively() {
        assert_eq!(extension_of("photo.PNG"), "png");
        assert_eq!(extension_of("archive.tar.gz"), "gz");
        assert_eq!(extension_of("noextension"), "bin");
        assert_eq!(extension_of("trailing."), "bin");
        // A very long "extension" is a filename with dots in it, not an extension.
        assert_eq!(extension_of("file.averyverylongextension"), "bin");
    }

    #[test]
    fn header_filenames_cannot_break_out_of_their_quotes() {
        assert_eq!(quoted(r#"evil".md"#), "evil_.md");
        assert_eq!(quoted(r"back\slash.md"), "back_slash.md");
        // Control characters are dropped, taking the header injection with them
        // while leaving the rest of the name readable.
        assert_eq!(quoted("newline\r\n.md"), "newline.md");
        assert_eq!(quoted("a\r\nSet-Cookie: x=y"), "aSet-Cookie: x=y");
        assert_eq!(quoted("ordinary name.md"), "ordinary name.md");
    }

    #[test]
    fn encodes_path_segments_but_keeps_separators() {
        assert_eq!(
            encode_path("attachments/2026/my file.png"),
            "attachments/2026/my%20file%2Epng"
        );
    }

    /// SVG can carry script, so it must never be in the inline-safe set even
    /// though it is an image people legitimately want to attach.
    #[test]
    fn svg_and_html_are_never_served_inline() {
        for dangerous in [
            "image/svg+xml",
            "text/html",
            "application/xhtml+xml",
            "text/xml",
            "application/xml",
            "application/javascript",
        ] {
            assert!(
                !INLINE_SAFE_MIMES.contains(&dangerous),
                "{dangerous} must not be inline-safe"
            );
        }
    }

    #[test]
    fn every_image_type_the_editor_embeds_is_also_inline_safe() {
        for mime in IMAGE_MIMES {
            assert!(
                INLINE_SAFE_MIMES.contains(mime),
                "{mime} is embedded by the editor but would not render inline"
            );
        }
    }
}
