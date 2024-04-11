use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use foundations::telemetry::log;
use serde::{Deserialize, Serialize};
use templater::State;

#[derive(Clone)]
pub struct ServerState {
    pub templater_state: Arc<State>,
    pub may_output_file: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct RenderResponse {}

#[derive(Debug)]
pub enum AppError {
    AnyError(anyhow::Error),
    NotAllowedOutput,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::AnyError(e) => {
                log::error!("{:?}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.")
            }
            Self::NotAllowedOutput => {
                log::error!("Output into file not allowed.");
                (StatusCode::BAD_REQUEST, "Invalid output.")
            }
        }
        .into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::AnyError(e)
    }
}
