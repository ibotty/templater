use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::Context;
use clap::Parser;
use foundations::{
    telemetry::{
        self,
        log::{self, debug},
        settings::{Level, TelemetrySettings},
    },
    BootstrapResult,
};

use templater::*;

#[derive(Debug, Parser)]
struct Cli {
    #[structopt(short, long, value_parser = TemplateRef::from_str)]
    template: TemplateRef,

    #[structopt(long)]
    templates_path: Option<PathBuf>,

    #[structopt(long)]
    assets_path: Option<PathBuf>,

    #[structopt(short, long)]
    inputs: Vec<FileRef>,

    #[structopt(short, long, value_parser = OutputRef::from_str)]
    output: OutputRef,

    #[structopt(short, long, action = clap::ArgAction::Count)]
    verbosity: u8,

    #[structopt(long)]
    disable_sandboxing: bool,
}

#[tokio::main]
async fn main() -> BootstrapResult<()> {
    let service_info = foundations::service_info!();
    let telemetry_settings = TelemetrySettings::default();
    telemetry::init(&service_info, &telemetry_settings)?;

    let opts = Cli::parse();

    let log_level =
        Level::from_usize((Level::Warning.as_usize() + opts.verbosity as usize).clamp(0, 5))
            .expect("could not set loglevel");
    log::set_verbosity(log_level).map_err(|e| anyhow::anyhow!("{:?}", e))?;

    debug!("parsed cli opts"; "opts" => format!("{:?}", opts));

    let template_path = Path::new(opts.template.as_ref());
    let this_template_dir = template_path
        .canonicalize()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    let template = TemplateRef::from_str(opts.template.as_ref().rsplit('/').next().unwrap())?;

    let assets_path = opts
        .assets_path
        .or_else(|| {
            this_template_dir
                .as_ref()
                .and_then(|dir| dir.parent().map(|p| p.join("assets")))
        })
        .unwrap_or_else(|| Path::new("./assets").to_path_buf())
        .canonicalize()?;

    let templates_path = opts
        .templates_path
        .or(this_template_dir)
        .unwrap_or_else(|| Path::new("./templates").to_path_buf());

    debug!("running with";
            "templates_path" => templates_path.as_path().display(),
            "assets_path" => assets_path.as_path().display(),
            "template" => template.as_ref(),
    );

    let state = State::new(templates_path, assets_path);
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
        enable_syscall_sandboxing(ViolationAction::AllowAndLog, &ALLOWED)
    }
}
