use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use clap::Parser;
use foundations::telemetry::TelemetryConfig;
use foundations::{
    BootstrapResult,
    telemetry::{
        self,
        log::{self, debug},
        settings::{Level, TelemetryServerSettings, TelemetrySettings, TracingSettings},
    },
};

use templater::*;

#[derive(Debug, Parser)]
struct Cli {
    #[arg(short, long)]
    template: PathBuf,

    #[arg(long)]
    templates_path: Option<PathBuf>,

    #[arg(long)]
    assets_path: Option<PathBuf>,

    #[arg(short, long)]
    inputs: Vec<FileRef>,

    #[arg(short, long, value_parser = OutputRef::from_str)]
    output: OutputRef,

    #[arg(short, long, action = clap::ArgAction::Count)]
    verbosity: u8,

    #[arg(long)]
    disable_sandboxing: bool,
}

#[tokio::main]
async fn main() -> BootstrapResult<()> {
    let service_info = foundations::service_info!();
    // only the logger is wanted; don't bind a telemetry server or set up a jaeger reporter
    let telemetry_settings = TelemetrySettings {
        server: TelemetryServerSettings {
            enabled: false,
            ..Default::default()
        },
        tracing: TracingSettings {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    let telemetry_config = TelemetryConfig {
        service_info: &service_info,
        settings: &telemetry_settings,
        custom_server_routes: vec![],
    };
    telemetry::init(telemetry_config)?;

    let opts = Cli::parse();

    // slog Level domain is 1..=6 (Critical=1 .. Trace=6).
    let log_level =
        Level::from_usize((Level::Warning.as_usize() + opts.verbosity as usize).clamp(1, 6))
            .expect("could not set loglevel");
    log::set_verbosity(log_level.into()).map_err(|e| anyhow::anyhow!("{:?}", e))?;

    debug!("parsed cli opts"; "opts" => format!("{:?}", opts));

    let template_path = opts.template.as_path();
    let this_template_dir = template_path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    let basename = template_path
        .file_name()
        .and_then(|s| s.to_str())
        .context("template path has no filename")?;
    let template = TemplateRef::try_new(basename.to_string())?;

    let assets_path = opts
        .assets_path
        .or_else(|| {
            this_template_dir
                .as_ref()
                .and_then(|dir| dir.parent().map(|p| p.join("assets")))
        })
        .unwrap_or_else(|| Path::new("./assets").to_path_buf())
        .canonicalize()
        .ok();

    let templates_path = opts
        .templates_path
        .or(this_template_dir)
        .unwrap_or_else(|| Path::new("./templates").to_path_buf());

    debug!("running with";
            "templates_path" => templates_path.as_path().display(),
            "assets_path" => assets_path.as_ref().map(|s| s.display()),
            "template" => template.as_ref(),
    );

    let state = State::new(templates_path, assets_path).context("Could not create state")?;
    let inputs = opts.inputs.into_iter().map(types::Input::FileRef).collect();

    let renderjob = RenderJob {
        output: opts.output,
        template,
        inputs,
    };

    let renderer = state
        .new_job(renderjob)
        .await
        .context("Could not create job")?;

    sandbox_syscalls(!opts.disable_sandboxing)?;

    renderer.run_job().await.context("Could not run job")?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn sandbox_syscalls(enabled: bool) -> BootstrapResult<()> {
    use foundations::security::{common_syscall_allow_lists::*, *};

    allow_list! {
        static ALLOWED = [
            ..ASYNC,
            ..SERVICE_BASICS,
            ..NET_SOCKET_API,
            ..ADDITIONAL_REQUIRED_SYSCALLS
        ]
    }
    if enabled {
        enable_syscall_sandboxing(ViolationAction::KillProcess, &ALLOWED)
    } else {
        // sysctl -n kernel.seccomp.actions_logged
        enable_syscall_sandboxing(ViolationAction::AllowAndLog, &ALLOWED)
    }
}
