pub mod filters;
pub mod s3;
pub mod types;

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use anyhow::Context;
use foundations::security::common_syscall_allow_lists::*;
use foundations::telemetry::log::debug;
use tokio::fs;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use anyhow::{Result, bail, ensure};
use async_tempfile::{Ownership, TempDir, TempFile};

pub use types::*;

/// Auto-escape ConTeXt/TeX templates so template *data* cannot inject control
/// sequences. `.mkiv`/`.tex` (also via a trailing `.j2`) => Custom("context");
/// everything else keeps minijinja's default (html/json/none). Structural
/// interpolations (filenames, setup keys) opt out with `| safe`.
fn configure_escaping(env: &mut minijinja::Environment<'static>) {
    env.set_auto_escape_callback(|name| match strip_jinja_ext(name).rsplit('.').next() {
        Some("mkiv" | "tex") => minijinja::AutoEscape::Custom("context"),
        _ => minijinja::default_auto_escape_callback(name),
    });
    env.set_formatter(|out, state, value| {
        if let minijinja::AutoEscape::Custom("context") = state.auto_escape() {
            // safe values (`|safe`, or macros returning safe strings) pass through
            if value.is_safe() {
                write!(out, "{value}")?;
            } else if let Some(s) = value.as_str() {
                out.write_str(&filters::context_escape(s))?;
            } else {
                // numbers/bool/none/etc: escape their string form defensively
                out.write_str(&filters::context_escape(&value.to_string()))?;
            }
            Ok(())
        } else {
            minijinja::escape_formatter(out, state, value)
        }
    });
}

/// Strip a trailing `.j2`/`.jinja`/`.jinja2` so `foo.mkiv.j2` is treated as `foo.mkiv`.
fn strip_jinja_ext(name: &str) -> &str {
    for ext in [".j2", ".jinja", ".jinja2"] {
        if let Some(stripped) = name.strip_suffix(ext) {
            return stripped;
        }
    }
    name
}

#[cfg(test)]
mod context_escape_tests {
    use super::configure_escaping;

    fn env() -> minijinja::Environment<'static> {
        let mut env = minijinja::Environment::new();
        configure_escaping(&mut env);
        env
    }

    #[test]
    fn escapes_data_in_mkiv() {
        let out = env()
            .render_named_str(
                "letter.mkiv",
                "{{ v }}",
                minijinja::context! { v => r"\input x" },
            )
            .unwrap();
        assert_eq!(out, r"\letterbackslash{}input x");
    }

    #[test]
    fn safe_opts_out() {
        let out = env()
            .render_named_str(
                "letter.mkiv",
                "{{ v | safe }}",
                minijinja::context! { v => "de_DE" },
            )
            .unwrap();
        assert_eq!(out, "de_DE");
    }

    #[test]
    fn non_tex_template_unescaped() {
        // .txt keeps default (no escaping) -> backslash passes through verbatim
        let out = env()
            .render_named_str("cano.txt", "{{ v }}", minijinja::context! { v => r"\x" })
            .unwrap();
        assert_eq!(out, r"\x");
    }
}

#[derive(Debug)]
pub struct State {
    reqwest_client: reqwest::Client,
    jinja_env: Arc<minijinja::Environment<'static>>,
    compile_semaphore: Arc<Semaphore>,
}

impl State {
    pub fn new(templates_path: impl AsRef<Path>, assets_path: Option<impl AsRef<Path>>) -> Self {
        let mut jinja_env = minijinja::Environment::new();

        jinja_env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);

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
        jinja_env.add_filter("qr_mp_pic", filters::qr_encode_to_mp_picture);

        configure_escaping(&mut jinja_env);
        jinja_env.set_loader(minijinja::path_loader(templates_path));

        let jinja_env = Arc::new(jinja_env);
        let reqwest_client = build_client();

        // Compiles are CPU-bound; cap concurrency so requests queue instead of
        // forking unbounded `context` processes. Override with MAX_CONCURRENT_COMPILES.
        let permits = std::env::var("MAX_CONCURRENT_COMPILES")
            .ok()
            .and_then(|v| v.parse().ok())
            .or_else(|| std::thread::available_parallelism().ok().map(Into::into))
            .unwrap_or(4);
        let compile_semaphore = Arc::new(Semaphore::new(permits));

        State {
            jinja_env,
            reqwest_client,
            compile_semaphore,
        }
    }

    pub async fn new_job(&self, job: RenderJob) -> Result<Renderer> {
        Renderer::setup(
            self.reqwest_client.clone(),
            self.jinja_env.clone(),
            self.compile_semaphore.clone(),
            job,
        )
        .await
    }
}

/// reqwest client with connect/request timeouts so a hung remote can't park a job.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()
        .expect("static reqwest config")
}

pub struct Renderer {
    dir: TempDir,
    reqwest_client: reqwest::Client,
    jinja_env: Arc<minijinja::Environment<'static>>,
    compile_semaphore: Arc<Semaphore>,
    template: TemplateRef,
    output: OutputRef,
    data: HashMap<String, minijinja::Value>,
}

impl Renderer {
    pub async fn setup(
        reqwest_client: reqwest::Client,
        jinja_env: Arc<minijinja::Environment<'static>>,
        compile_semaphore: Arc<Semaphore>,
        job: RenderJob,
    ) -> Result<Self> {
        let dir = TempDir::new().await?;

        let mut data: HashMap<String, minijinja::Value> = Default::default();
        for input in job.inputs.into_iter() {
            data.extend(input.read_into_env(&reqwest_client).await?);
        }

        Ok(Self {
            dir,
            reqwest_client,
            jinja_env,
            compile_semaphore,
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
            let _permit = self
                .compile_semaphore
                .acquire()
                .await
                .expect("compile semaphore closed");
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
                    let mut stdout = io::stdout();
                    io::copy(&mut output_file, &mut stdout)
                        .await
                        .context("Could not write to stdout")?;
                    stdout.flush().await.context("Could not flush stdout")?;
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
            TempFile::new_with_name_in(self.template.as_ref(), self.dir.dir_path().to_owned())
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

        // env-tunable; generous default for complex ConTeXt runs.
        let timeout_secs = std::env::var("CONTEXT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        // spawn (not output()) so kill_on_drop reaps the child if we time out.
        let child = Command::new("context")
            .arg("--batchmode")
            .arg(path)
            .current_dir(&self.dir)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("Could not spawn command")?;

        let context_proc =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait_with_output())
                .await
            {
                Ok(res) => res.context("Could not run command")?,
                Err(_elapsed) => bail!("compilation timed out after {timeout_secs}s"),
            };
        let status = context_proc.status;

        debug!("ran pdf compilation"; "status" => status.code(), "signal" => status.signal(), "core_dumped" => status.core_dumped(), "stopped_signal" => status.stopped_signal());
        debug!(
            "stdout: {:?}",
            String::from_utf8_lossy(&context_proc.stdout)
        );
        debug!(
            "stderr: {:?}",
            String::from_utf8_lossy(&context_proc.stderr)
        );

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
        ..RUST_BASICS,
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
        getresgid,
        getresuid,
        gettimeofday,
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
