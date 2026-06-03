//! HTTP error type and `IntoResponse` mapping.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;

/// All errors that can be returned from an axum handler. Auto-maps to
/// an HTTP response with a JSON body `{error: {code, message}}` and the
/// appropriate status code.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
    #[error("conflict: {0}")]
    Conflict(String),
}

impl ApiError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Conflict(_) => StatusCode::CONFLICT,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Internal(_) => "internal_error",
            Self::Conflict(_) => "conflict",
        }
    }
}

#[derive(Serialize)]
struct ErrorBody {
    error: ErrorBodyInner,
}

#[derive(Serialize)]
struct ErrorBodyInner {
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorBodyInner {
                code: self.code(),
                message: self.to_string(),
            },
        };
        (self.status(), Json(body)).into_response()
    }
}

// --- Mappings from existing error types ---

impl From<crate::skills::SkillError> for ApiError {
    fn from(e: crate::skills::SkillError) -> Self {
        use crate::skills::SkillError;
        match e {
            SkillError::NotFound(s) => Self::NotFound(s),
            SkillError::InvalidSlug(s) => Self::BadRequest(s),
            SkillError::Io(_) | SkillError::Serde(_) | SkillError::Model(_) | SkillError::Harness(_) => {
                Self::Internal(e.to_string())
            }
        }
    }
}

impl From<crate::error::HarnessError> for ApiError {
    fn from(e: crate::error::HarnessError) -> Self {
        Self::Internal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_status_is_404() {
        assert_eq!(ApiError::NotFound("x".into()).status(), StatusCode::NOT_FOUND);
        assert_eq!(ApiError::NotFound("x".into()).code(), "not_found");
    }

    #[test]
    fn bad_request_status_is_400() {
        assert_eq!(ApiError::BadRequest("x".into()).status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn internal_status_is_500() {
        assert_eq!(ApiError::Internal("x".into()).status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn skill_error_not_found_maps_to_api_404() {
        let e = crate::skills::SkillError::NotFound("foo".into());
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn skill_error_invalid_slug_maps_to_api_400() {
        let e = crate::skills::SkillError::InvalidSlug("bad!!".into());
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn skill_error_io_maps_to_api_500() {
        let e = crate::skills::SkillError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound, "missing",
        ));
        let api: ApiError = e.into();
        assert_eq!(api.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
