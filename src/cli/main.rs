use std::{collections::HashMap, path::PathBuf, str::FromStr};

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

    #[structopt(long, default_value = "examples/templates")]
    templates_path: PathBuf,

    #[structopt(short, long)]
    inputs: Vec<InputRef>,

    #[structopt(short, long, value_parser = OutputRef::from_str)]
    output: OutputRef,

    #[structopt(short, long, action = clap::ArgAction::Count)]
    verbosity: u8,
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

    debug!("parsed options: {:?}", opts);

    let data = HashMap::new();
    let renderjob = RenderJob {
        data,
        output: opts.output,
        template: opts.template,
    };
    let state = State::new(opts.templates_path);
    let renderer = state
        .new_job(renderjob)
        .await
        .context("Could not create job")?;

    sandbox_syscalls()?;

    renderer.run_job().await.context("Could not run job")?;
    Ok(())
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
