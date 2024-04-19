pub mod filters;
pub mod s3;
pub mod types;

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Context;
use foundations::security::common_syscall_allow_lists::*;
use foundations::telemetry::log::debug;
use minijinja::Environment;
use tokio::fs;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use anyhow::{ensure, Result};
use async_tempfile::{Ownership, TempFile};
use tempfile::TempDir;

pub use types::*;

#[derive(Debug)]
pub struct State {
    reqwest_client: OnceLock<reqwest::Client>,
    jinja_env: Arc<Environment<'static>>,
}

impl State {
    pub fn new(templates_path: impl AsRef<Path>, assets_path: Option<impl AsRef<Path>>) -> Self {
        let mut jinja_env = Environment::new();
        if let Some(assets_path) = assets_path {
            jinja_env.add_global("__assets_path", assets_path.as_ref().to_str().unwrap());
        }

        jinja_env.add_global(
            "__templates_path",
            templates_path.as_ref().to_str().unwrap(),
        );
        jinja_env.add_filter("currency_format", filters::currency_format);
        jinja_env.add_filter("split", filters::split);
        jinja_env.add_filter("context_escape", filters::context_escape);
        jinja_env.set_loader(minijinja::path_loader(templates_path));

        let jinja_env = Arc::new(jinja_env);
        let reqwest_client = OnceLock::new();

        State {
            jinja_env,
            reqwest_client,
        }
    }

    pub async fn new_job(&self, job: RenderJob) -> Result<Renderer> {
        Renderer::setup(
            self.reqwest_client
                .get_or_init(reqwest::Client::new)
                .clone(),
            self.jinja_env.clone(),
            job,
        )
        .await
    }
}

pub struct Renderer {
    dir: TempDir,
    reqwest_client: reqwest::Client,
    jinja_env: Arc<Environment<'static>>,
    template: TemplateRef,
    output: OutputRef,
    data: HashMap<String, minijinja::Value>,
}

impl Renderer {
    pub async fn setup<'a>(
        reqwest_client: reqwest::Client,
        jinja_env: Arc<Environment<'static>>,
        job: RenderJob,
    ) -> Result<Self> {
        let dir = TempDir::new()?;

        let mut data: HashMap<String, minijinja::Value> = Default::default();
        for input in job.inputs.into_iter() {
            data.extend(input.read_into_env(&reqwest_client).await?.into_iter());
        }

        Ok(Self {
            dir,
            reqwest_client,
            jinja_env,
            data,
            template: job.template,
            output: job.output,
        })
    }

    pub async fn run_job(&self) -> Result<Option<OutputBuffer>> {
        let mut output_file = self
            .write_template()
            .await
            .context("Could not create template")?;

        if self.template.should_compile() {
            output_file = self
                .compile_pdf(&output_file)
                .await
                .context("Could not compile pdf")?;
        }
        let mime_type = self.template.mime_type();

        match &self.output {
            OutputRef::File(FileRef::Url(url)) => {
                s3::upload_file(&self.reqwest_client, output_file, mime_type, url.clone())
                    .await
                    .context("Could not upload file")?;
                Ok(None)
            }
            OutputRef::File(FileRef::File(filename)) => {
                if filename.as_os_str() == "-" {
                    let mut buf: [u8; 64] = [0; 64];
                    let mut stdout = io::stdout();
                    loop {
                        let n = output_file
                            .read(&mut buf)
                            .await
                            .context("Could not read from file")?;
                        if n == 0 {
                            break;
                        }
                        stdout
                            .write(&buf[0..n])
                            .await
                            .context("Could not write to stdout")?;
                    }
                } else {
                    let _ = fs::copy(output_file.file_path(), filename)
                        .await
                        .context("Could not copy file")?;
                }
                Ok(None)
            }
            OutputRef::Buffer => {
                // unwrap is safe, because it's no directory
                let filename = output_file
                    .file_path()
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_string();

                let mut buffer = vec![];
                output_file
                    .read_to_end(&mut buffer)
                    .await
                    .context("Could not read from file")?;

                Ok(Some(OutputBuffer {
                    buffer,
                    filename,
                    mime_type,
                }))
            }
        }
    }

    pub async fn write_template(&self) -> Result<TempFile> {
        let templated_file =
            TempFile::new_with_name_in(self.template.as_ref(), self.dir.path().to_owned())
                .await
                .context("Could not create template file")?;

        let rendered = self
            .jinja_env
            .get_template(self.template.as_ref())
            .context("Could not get template")?
            .render(&self.data)
            .context("Could not render template")?;
        let mut f = templated_file
            .open_rw()
            .await
            .context("Could not open templated_file rw")?;
        f.write_all(rendered.as_bytes())
            .await
            .context("Could not write rendered template")?;
        Ok(templated_file)
    }

    pub async fn compile_pdf(&self, file: &TempFile) -> Result<TempFile> {
        // create TempFile but with .pdf extension
        let path = file.file_path();
        let output_file_name: &Path = path.file_stem().unwrap().as_ref();
        let output_file_name = output_file_name.with_extension("pdf");
        let output_file_path = path.with_file_name(output_file_name);
        debug!("trying to compile"; "template-file" => path.to_str(), "output-file" => output_file_path.to_str());

        let command = "context";
        let context_proc = Command::new(command)
            .arg("--batchmode")
            .arg(path)
            .current_dir(&self.dir)
            .output()
            .await
            .context("Could not spawn command")?;
        let status = context_proc.status;

        debug!("ran pdf compilation"; "status" => status.code());
        debug!("stdout: {:?}", String::from_utf8_lossy(&context_proc.stdout));
        debug!("stderr: {:?}", String::from_utf8_lossy(&context_proc.stderr));

        ensure!(status.success(), "Could not compile file");

        let output_file = TempFile::from_existing(output_file_path, Ownership::Owned)
            .await
            .context("Could not open existing file as tempfile")?;
        Ok(output_file)
    }
}

#[cfg(target_os = "linux")]
foundations::security::allow_list! {
    pub static ADDITIONAL_REQUIRED_SYSCALLS = [
        ..ASYNC,
        ..SERVICE_BASICS,
        access,
        arch_prctl,
        chdir,
        clock_gettime,
        copy_file_range,
        dup2,
        dup3,
        execve,
        fchmod,
        fcntl,
        getcwd,
        getdents64,
        getegid,
        geteuid,
        getgid,
        getpgrp,
        getppid,
        getuid,
        openat,
        pidfd_open,
        pipe2,
        pread64,
        prlimit64,
        readlink,
        rename,
        rt_sigaction,
        rt_sigreturn,
        set_tid_address,
        sysinfo,
        uname,
        unlink,
        unlinkat,
        wait4
    ]
}
