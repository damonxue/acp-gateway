//! One HTTP error shape, derived from [`ErrorKind`].

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use gateway_core::error::{ErrorKind, GatewayError};
use serde::Serialize;
use tracing::warn;

/// A gateway error on its way out through HTTP.
#[derive(Debug)]
pub struct ApiError(pub GatewayError);

impl From<GatewayError> for ApiError {
    fn from(error: GatewayError) -> Self {
        Self(error)
    }
}

/// The JSON body every failing request returns.
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    /// Stable machine-readable code, e.g. `session_not_found`.
    pub code: &'static str,
    /// Human-readable description. Safe to show a user.
    pub message: String,
}

/// Translate a domain error kind into a status code.
///
/// The table lives here, once, because "which status does this error get?" is
/// a transport decision and every handler must answer it the same way.
#[must_use]
pub fn status_for(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
        ErrorKind::Forbidden => StatusCode::FORBIDDEN,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::Conflict => StatusCode::CONFLICT,
        ErrorKind::Expired => StatusCode::GONE,
        ErrorKind::RateLimited => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorKind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = status_for(self.0.kind());
        if status.is_server_error() {
            warn!(error = %self.0, "request failed");
        }
        (
            status,
            Json(ErrorBody {
                code: self.0.code(),
                message: self.0.to_string(),
            }),
        )
            .into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gateway_core::ids::SessionId;

    #[test]
    fn the_documented_status_mapping_holds() {
        assert_eq!(
            status_for(GatewayError::SessionNotFound(SessionId::new("x")).kind()),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_for(GatewayError::Expired("ticket".into()).kind()),
            StatusCode::GONE
        );
        assert_eq!(
            status_for(GatewayError::AuthenticationFailed("no".into()).kind()),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            status_for(GatewayError::AgentUnavailable("gone".into()).kind()),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
