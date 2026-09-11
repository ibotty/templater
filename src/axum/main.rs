mod types;

use std::env;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{self, ConnectInfo};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use foundations::BootstrapResult;
use foundations::cli::{Arg, ArgAction, Cli};
use foundations::telemetry::TelemetryConfig;
use foundations::telemetry::{
    self,
    log::{self, trace},
    settings::TelemetrySettings,
};
use reqwest::header;
use tokio::net::TcpListener;
use tokio::signal::unix;

use crate::types::*;
use templater::*;

#[tokio::main]
async fn main() -> BootstrapResult<()> {
    let service_info = foundations::service_info!();
    let cli = Cli::<TelemetrySettings>::new(
        &service_info,
        vec![
            Arg::new("check")
                .long("check")
                .action(ArgAction::SetTrue)
                .help("Validate config."),
        ],
    )?;

    if cli.arg_matches.get_flag("check") {
        return Ok(());
    }

    let telemetry_fut = telemetry::init(TelemetryConfig {
        service_info: &service_info,
        settings: &cli.settings,
        custom_server_routes: vec![],
    })?;
    if let Some(addr) = telemetry_fut.server_addr() {
        log::info!("Telemetry server listening on http://{}", addr);
    }

    let templates_path = env::var("TEMPLATES_PATH").unwrap_or("./templates".to_string());
    let assets_path = Path::new(&env::var("ASSETS_PATH").unwrap_or("./assets".to_string()))
        .canonicalize()
        .ok();
    let templater_state = Arc::new(State::new(templates_path, assets_path));
    let may_output_file = env_flag("MAY_OUTPUT_TO_FILE");
    let may_input_file = env_flag("MAY_INPUT_FROM_FILE");
    let server_state = ServerState {
        templater_state,
        may_output_file,
        may_input_file,
    };

    let bind_addr = "0.0.0.0:8080";

    let app = axum::Router::new()
        .route("/", axum::routing::post(post_renderjob))
        .route("/_healthz", axum::routing::get(healthz))
        .with_state(server_state);
    let listener = TcpListener::bind(bind_addr).await?;
    let axum_fut = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .into_future();

    log::info!("Server listening on http://{}", bind_addr);

    #[cfg(target_os = "linux")]
    sandbox_syscalls()?;

    tokio::select! {
        r = telemetry_fut => { r? },
        r = axum_fut => { r? },
    }

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    let terminate = async {
        unix::signal(unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    log::info!("signal received, starting graceful shutdown");
}

#[axum::debug_handler]
async fn healthz() -> &'static str {
    "OK\n"
}

/// An env var is "set" only for truthy values, not merely presence.
fn env_flag(name: &str) -> bool {
    matches!(env::var(name).as_deref(), Ok("1" | "true" | "yes"))
}

/// Reject file-path inputs/outputs unless explicitly allowed. Stdin (`-`) is
/// never reachable over HTTP (it would read the server's own stdin).
fn gate_file_io(
    may_output_file: bool,
    may_input_file: bool,
    job: &RenderJob,
) -> Result<(), AppError> {
    if !may_output_file && matches!(job.output, OutputRef::File(FileRef::File(_))) {
        return Err(AppError::NotAllowedOutput);
    }
    for input in &job.inputs {
        if let Input::FileRef(FileRef::File(path)) = input
            && (path.as_os_str() == "-" || !may_input_file)
        {
            return Err(AppError::NotAllowedInput);
        }
    }
    Ok(())
}

#[axum::debug_handler]
async fn post_renderjob(
    state: axum::extract::State<ServerState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    extract::Json(renderjob): extract::Json<RenderJob>,
) -> Result<impl IntoResponse, AppError> {
    trace!("got request"; "client-ip" => format!("{}", client_addr.ip()));

    gate_file_io(state.may_output_file, state.may_input_file, &renderjob)?;

    let renderer = state.templater_state.new_job(renderjob).await?;
    match renderer.run_job().await? {
        None => Ok(StatusCode::OK.into_response()),
        Some(output) => {
            let headers = [
                (
                    header::CONTENT_TYPE,
                    output.mime_type.essence_str().to_string(),
                ),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}\"", output.filename),
                ),
            ];

            Ok((headers, output.buffer).into_response())
        }
    }
}

#[cfg(target_os = "linux")]
fn sandbox_syscalls() -> BootstrapResult<()> {
    use foundations::security::{common_syscall_allow_lists::*, *};

    allow_list! {
        static ALLOWED = [
            ..ASYNC,
            ..SERVICE_BASICS,
            ..NET_SOCKET_API,
            ..ADDITIONAL_REQUIRED_SYSCALLS
        ]
    }
    enable_syscall_sandboxing(ViolationAction::KillProcess, &ALLOWED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::str::FromStr;

    fn job(inputs: Vec<Input>, output: OutputRef) -> RenderJob {
        RenderJob {
            template: TemplateRef::try_new("t.mkiv".to_string()).unwrap(),
            output,
            inputs,
        }
    }

    #[test]
    fn file_input_rejected_by_default() {
        let j = job(
            vec![Input::FileRef(FileRef::from("/etc/passwd"))],
            OutputRef::Buffer,
        );
        assert!(matches!(
            gate_file_io(false, false, &j),
            Err(AppError::NotAllowedInput)
        ));
        // allowed when flag set
        assert!(gate_file_io(false, true, &j).is_ok());
    }

    #[test]
    fn stdin_input_always_rejected() {
        let j = job(vec![Input::FileRef(FileRef::from("-"))], OutputRef::Buffer);
        // even with may_input_file = true
        assert!(matches!(
            gate_file_io(false, true, &j),
            Err(AppError::NotAllowedInput)
        ));
    }

    #[test]
    fn inline_and_url_inputs_pass() {
        let j = job(vec![Input::Inline(HashMap::new())], OutputRef::Buffer);
        assert!(gate_file_io(false, false, &j).is_ok());
    }

    #[test]
    fn file_output_gated() {
        let j = job(vec![], OutputRef::from_str("/tmp/x.pdf").unwrap());
        assert!(matches!(
            gate_file_io(false, false, &j),
            Err(AppError::NotAllowedOutput)
        ));
        assert!(gate_file_io(true, false, &j).is_ok());
    }
}
