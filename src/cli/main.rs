use std::{collections::HashMap, path::PathBuf, str::FromStr};

use anyhow::{bail, Context};
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

    #[structopt(long, default_value = "examples/assets")]
    assets_path: PathBuf,

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

    let mut data = HashMap::new();
    for input in opts.inputs {
        let filename = input.as_ref();
        let bytes = tokio::fs::read(filename)
            .await
            .with_context(|| format!("Cannot open input file {}", filename.display()))?;
        let new_data: HashMap<String, minijinja::Value> =
            match filename.extension().and_then(|s| s.to_str()) {
                Some("json") => serde_json::from_slice(&bytes)?,
                Some("yaml") => serde_yaml::from_slice(&bytes)?,
                _ => bail!("Unsupported input file {}", filename.display()),
            };
        data.extend(new_data.into_iter());
    }
    let renderjob = RenderJob {
        data,
        output: opts.output,
        template: opts.template,
    };
    let state = State::new(opts.templates_path, opts.assets_path);
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
