use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use foundations::telemetry::log;
use serde::{Deserialize, Serialize};
use templater::State;

pub type ServerState = Arc<State>;

#[derive(Debug, Deserialize, Serialize)]
pub struct RenderResponse {}

pub enum AppError {
    AnyError(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::AnyError(e) => {
                log::error!("{}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.")
            }
        }
        .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::AnyError(err.into())
    }
}
