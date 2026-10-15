use std::fmt;

use actix_web::error::InternalError;
use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};

use crate::ErrorResponse;

use vmdesk_db::Error;

///Maps database errors to HTTP: 404, 409, 422, else 500 with no internal details.
#[derive(Debug)]
pub struct ApiError(pub Error);

impl From<Error> for ApiError {
    fn from(e: Error) -> Self {
        Self(e)
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ResponseError for ApiError {
    fn status_code(&self) -> StatusCode {
        match self.0 {
            Error::NotFound => StatusCode::NOT_FOUND,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Invalid(_) => StatusCode::UNPROCESSABLE_ENTITY,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        let status = self.status_code();
        let (code, message) = match &self.0 {
            Error::NotFound => ("not found", "not found".to_owned()),
            Error::Conflict(m) => ("conflict", m.clone()),
            Error::Invalid(m) => ("invalid", m.clone()),
            other => {
                tracing::error!(error = %other, "request failed");
                ("internal", "internal_error".to_owned())
            }
        };
        HttpResponse::build(status).json(ErrorResponse {
            error: code.to_string(),
            message: message.to_string(),
        })
    }
}

/// Turns a body, path or query extraction failure into a JSON 400.
pub(crate) fn bad_request(err: impl fmt::Display) -> actix_web::Error {
    let message = err.to_string();
    let response = HttpResponse::BadRequest().json(ErrorResponse {
        error: "bad_request".to_string(),
        message: message.clone(),
    });
    InternalError::from_response(message, response).into()
}
