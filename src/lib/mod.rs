pub mod filters;
pub mod s3;
pub mod types;

use std::path::Path;
use std::sync::Arc;
use std::sync::OnceLock;

use anyhow::Context;
use foundations::security::common_syscall_allow_lists::*;
use foundations::telemetry::log::debug;
use minijinja::Environment;
use tokio::{fs, io::AsyncWriteExt, process::Command};

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
    pub fn new(templates_path: impl AsRef<Path>, assets_path: impl AsRef<Path>) -> Self {
        let mut jinja_env = Environment::new();
        jinja_env.add_global("__assets_path", assets_path.as_ref().to_str().unwrap());
        jinja_env.add_global(
            "__templates_path",
            templates_path.as_ref().to_str().unwrap(),
        );
        jinja_env.add_filter("currency_format", filters::currency_format);
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
    job: RenderJob,
    dir: TempDir,
    reqwest_client: reqwest::Client,
    jinja_env: Arc<Environment<'static>>,
}

impl Renderer {
    pub async fn setup<'a>(
        reqwest_client: reqwest::Client,
        jinja_env: Arc<Environment<'static>>,
        job: RenderJob,
    ) -> Result<Self> {
        let dir = TempDir::new()?;

        Ok(Self {
            job,
            dir,
            reqwest_client,
            jinja_env,
        })
    }

    pub async fn run_job(&self) -> Result<()> {
        let mut output_file = self
            .write_template()
            .await
            .context("Could not create template")?;

        if self.job.template.should_compile() {
            output_file = self
                .compile_pdf(&output_file)
                .await
                .context("Could not compile pdf")?;
        }

        match self.job.output.as_ref() {
            FileRef::Url(url) => s3::upload_file(
                &self.reqwest_client,
                output_file,
                self.job.template.mime_type(),
                url.clone(),
            )
            .await
            .context("Could not upload file")?,
            FileRef::File(file) => {
                let _ = fs::copy(output_file.file_path(), file)
                    .await
                    .context("Could not copy file")?;
            }
        }
        Ok(())
    }

    pub async fn write_template(&self) -> Result<TempFile> {
        let templated_file =
            TempFile::new_with_name_in(self.job.template.as_ref(), self.dir.path().to_owned())
                .await
                .context("Could not create template file")?;
        let rendered = self
            .jinja_env
            .get_template(self.job.template.as_ref())
            .context("Could not get template")?
            .render(&self.job.data)
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
            .spawn()
            .context("Could not spawn command")?;
        let cmd_output = context_proc
            .wait_with_output()
            .await
            .context("Could not get command output")?;
        let stdout = String::from_utf8(cmd_output.stdout)?;
        let stderr = String::from_utf8(cmd_output.stderr)?;
        let status = cmd_output.status;

        debug!("ran pdf compilation"; "status" => status.code(), "stdout" => stdout, "stderr" => stderr);
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
