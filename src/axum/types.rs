use std::sync::Arc;

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use foundations::telemetry::log;
use templater::State;

#[derive(Clone)]
pub struct ServerState {
    pub templater_state: Arc<State>,
    pub may_output_file: bool,
    pub may_input_file: bool,
}

#[derive(Debug)]
pub enum AppError {
    AnyError(anyhow::Error),
    NotAllowedOutput,
    NotAllowedInput,
    TemplateNotFound(String),
    MissingField(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::AnyError(e) => {
                log::error!("{:?}", e);
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Something went wrong.".to_string(),
                )
            }
            Self::NotAllowedOutput => {
                log::error!("Output into file not allowed.");
                (StatusCode::BAD_REQUEST, "Invalid output.".to_string())
            }
            Self::NotAllowedInput => {
                log::error!("Input from file not allowed.");
                (StatusCode::BAD_REQUEST, "Invalid input.".to_string())
            }
            Self::TemplateNotFound(msg) => {
                log::error!("{}", msg);
                (StatusCode::NOT_FOUND, msg)
            }
            Self::MissingField(msg) => {
                log::error!("{}", msg);
                (StatusCode::BAD_REQUEST, msg)
            }
        }
        .into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        // minijinja errors carry a Display message meaningful enough to return
        // directly (template name, missing-field context, line info).
        match e.chain().find_map(|c| c.downcast_ref::<minijinja::Error>()) {
            Some(me) => match me.kind() {
                minijinja::ErrorKind::TemplateNotFound => {
                    AppError::TemplateNotFound(me.to_string())
                }
                minijinja::ErrorKind::UndefinedError => AppError::MissingField(me.to_string()),
                _ => AppError::AnyError(e),
            },
            None => AppError::AnyError(e),
        }
    }
}

#[cfg(test)]
mod error_mapping_tests {
    use super::*;

    #[test]
    fn template_not_found_is_meaningful() {
        let e = anyhow::Error::new(minijinja::Error::from(
            minijinja::ErrorKind::TemplateNotFound,
        ))
        .context("Could not get template");
        assert!(matches!(AppError::from(e), AppError::TemplateNotFound(_)));
    }

    #[test]
    fn missing_field_is_meaningful() {
        let e = anyhow::Error::new(minijinja::Error::from(minijinja::ErrorKind::UndefinedError))
            .context("Could not render template");
        assert!(matches!(AppError::from(e), AppError::MissingField(_)));
    }

    #[test]
    fn other_errors_stay_generic() {
        let e = anyhow::anyhow!("boom");
        assert!(matches!(AppError::from(e), AppError::AnyError(_)));
    }
}
