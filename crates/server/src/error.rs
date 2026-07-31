//! The single error type every handler returns.
//!
//! Two rules hold throughout: the wire format is always [`ApiError`], and the
//! `message` field is safe to show a user. Anything that might carry internal
//! detail — a sqlx error, an io error with an absolute path in it — is logged
//! at `error` level and replaced with a generic message, so the filesystem
//! layout never leaks to the client.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use go_notes_shared::paths::PathError;
use go_notes_shared::{ApiError, ConflictBody};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found")]
    NotFound,

    #[error("{0}")]
    InvalidPath(#[from] PathError),

    /// Something exists where the caller wanted to create something.
    #[error("{0}")]
    AlreadyExists(String),

    /// A save lost its If-Match check. Carries the current file so the client
    /// can present a choice without a follow-up request.
    #[error("the note changed on disk")]
    Conflict {
        current_markdown: String,
        current_hash: String,
    },

    #[error("{0}")]
    BadRequest(String),

    #[error("authentication required")]
    Unauthenticated,

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    TooLarge(String),

    #[error("{0}")]
    UnsupportedMedia(String),

    /// Anything unexpected. The inner error is logged, never sent.
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl AppError {
    pub fn bad_request(msg: impl Into<String>) -> Self {
        AppError::BadRequest(msg.into())
    }

    pub fn forbidden(msg: impl Into<String>) -> Self {
        AppError::Forbidden(msg.into())
    }

    fn code(&self) -> &'static str {
        match self {
            AppError::NotFound => "not_found",
            AppError::InvalidPath(_) => "invalid_path",
            AppError::AlreadyExists(_) => "already_exists",
            AppError::Conflict { .. } => "conflict",
            AppError::BadRequest(_) => "bad_request",
            AppError::Unauthenticated => "unauthenticated",
            AppError::Forbidden(_) => "forbidden",
            AppError::TooLarge(_) => "too_large",
            AppError::UnsupportedMedia(_) => "unsupported_media",
            AppError::Internal(_) => "internal",
        }
    }

    fn status(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::InvalidPath(_) | AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::AlreadyExists(_) | AppError::Conflict { .. } => StatusCode::CONFLICT,
            AppError::Unauthenticated => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) => StatusCode::FORBIDDEN,
            AppError::TooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::UnsupportedMedia(_) => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        let code = self.code().to_string();

        // The conflict case has its own richer body.
        if let AppError::Conflict {
            current_markdown,
            current_hash,
        } = self
        {
            return (
                status,
                Json(ConflictBody {
                    code,
                    message: "This note changed on disk since you opened it.".into(),
                    current_markdown,
                    current_hash,
                }),
            )
                .into_response();
        }

        let message = match &self {
            AppError::Internal(err) => {
                // The only place internal detail is allowed to go is the log.
                tracing::error!(error = ?err, "unhandled internal error");
                "Something went wrong on the server.".to_string()
            }
            other => other.to_string(),
        };

        (status, Json(ApiError { code, message })).into_response()
    }
}

/// sqlx errors are always internal — a failed query is never the caller's fault
/// in a way we want to describe to them.
impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        AppError::Internal(anyhow::Error::new(err))
    }
}

/// io errors map to `NotFound` only for a genuinely missing file; everything
/// else (permissions, no space, a broken symlink) is a server problem.
impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        match err.kind() {
            std::io::ErrorKind::NotFound => AppError::NotFound,
            _ => AppError::Internal(anyhow::Error::new(err)),
        }
    }
}
