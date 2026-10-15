use actix_web::body::MessageBody;
use actix_web::dev::{ServiceRequest, ServiceResponse};
use actix_web::http::header;
use actix_web::middleware::Next;
use actix_web::{Error, HttpResponse, web};

use crate::{AppState, ErrorResponse};

pub(crate) async fn require_token(
    req: ServiceRequest,
    next: Next<impl MessageBody + 'static>,
) -> Result<ServiceResponse<impl MessageBody>, Error> {
    if req.path() == "/api/v1/health" || authorized(&req) {
        return next
            .call(req)
            .await
            .map(ServiceResponse::map_into_left_body);
    }
    let response = HttpResponse::Unauthorized()
        .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
        .json(ErrorResponse {
            error: "unauthorized".to_string(),
            message: "missing or invalid token".to_string(),
        });
    Ok(req.into_response(response).map_into_right_body())
}

fn authorized(req: &ServiceRequest) -> bool {
    let Some(state) = req.app_data::<web::Data<AppState>>() else {
        return false;
    };

    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|t| constant_time_eq(t.trim().as_bytes(), state.token.as_bytes()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
