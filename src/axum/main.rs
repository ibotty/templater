mod types;

use std::env;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{self, ConnectInfo};
use axum::response::IntoResponse;
use axum::Json;
use foundations::cli::{Arg, ArgAction, Cli};
use foundations::telemetry::{
    self,
    log::{self, trace},
    settings::TelemetrySettings,
};
use foundations::BootstrapResult;
use mime_guess::mime;
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
        vec![Arg::new("check")
            .long("check")
            .action(ArgAction::SetTrue)
            .help("Validate config.")],
    )?;

    if cli.arg_matches.get_flag("check") {
        return Ok(());
    }

    let telemetry_fut = telemetry::init_with_server(&service_info, &cli.settings, vec![])?;
    if let Some(addr) = telemetry_fut.server_addr() {
        log::info!("Telemetry server listening on http://{}", addr);
    }

    let templates_path = env::var("TEMPLATES_PATH").unwrap_or("./templates".to_string());
    let assets_path = env::var("ASSETS_PATH").unwrap_or("./assets".to_string());
    let templater_state = Arc::new(State::new(templates_path, assets_path));
    let may_output_file = env::var("MAY_OUTPUT_TO_FILE").is_ok();
    let server_state = ServerState {
        templater_state,
        may_output_file,
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

#[axum::debug_handler]
async fn post_renderjob(
    state: axum::extract::State<ServerState>,
    ConnectInfo(client_addr): ConnectInfo<SocketAddr>,
    extract::Json(renderjob): extract::Json<RenderJob>,
) -> Result<impl IntoResponse, AppError> {
    trace!("got request"; "client-ip" => format!("{}", client_addr.ip()));

    if !state.may_output_file {
        if let OutputRef::File(FileRef::File(_file)) = renderjob.output {
            return Err(AppError::NotAllowedOutput);
        }
    }

    let renderer = state.templater_state.new_job(renderjob).await?;
    match renderer.run_job().await? {
        None => {
            let response = RenderResponse {};
            Ok(Json(response).into_response())
        }
        Some(output) => {
            let headers = [
                (header::CONTENT_TYPE, mime::APPLICATION_PDF.essence_str()),
                //(header::CONTENT_TYPE, output.mime_type.essence_str()),
                //(
                //    header::CONTENT_DISPOSITION,
                //    "attachment; filename=\"Cargo.toml\"",
                //),
            ];

            Ok((headers, output).into_response())
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
