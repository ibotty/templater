pub mod filters;
pub mod s3;
pub mod types;

use std::collections::HashMap;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
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

/// Scans the first `MAX_LINES` lines for `templater:` and parses `key="value"` pairs.
/// Values MUST be double-quoted: `<!-- templater: title="Hello World" -->`, `% templater: lang="en"`.
pub fn parse_magic(content: &str) -> HashMap<String, String> {
    const MARKER: &str = "templater:";
    const MAX_LINES: usize = 5;

    content
        .lines()
        .take(MAX_LINES)
        .filter_map(|l| l.split_once(MARKER))
        .flat_map(|(_, rest)| {
            // Chunks alternate: `key=`, value, `key=`, value, ..., trailing junk (`-->`).
            // ponytail: no escapes — a `\"` inside a value ends it. Add a scanner if that shows up.
            let mut parts = rest.split('"');
            std::iter::from_fn(move || {
                let k = parts.next()?.trim().trim_end_matches('=').trim();
                Some((k.to_string(), parts.next()?.to_string()))
            })
        })
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

#[test]
fn magic() {
    let m = parse_magic("<!-- templater: title=\"Hello World\" lang=\"en\" -->\n<p>body</p>\n");
    assert_eq!(m["title"], "Hello World");
    assert_eq!(m["lang"], "en");

    assert_eq!(parse_magic("% templater: a=\"1\"")["a"], "1");
    assert_eq!(parse_magic("% templater: p=\"a.tex\"")["p"], "a.tex"); // trailing dot-path, no closer
    assert!(parse_magic("<!-- templater: a=1 -->").is_empty()); // unquoted rejected
    assert!(parse_magic("no markers here").is_empty());
    assert!(parse_magic(&("\n".repeat(9) + "% templater: a=\"1\"")).is_empty()); // too deep
}

/// A syntax definition as written in a `syntax/*.yaml` / `syntax/*.json` file.
/// Every field is optional; omitted ones keep minijinja's default delimiters.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SyntaxDef {
    block: Option<(String, String)>,
    variable: Option<(String, String)>,
    comment: Option<(String, String)>,
    line_statement_prefix: Option<String>,
    line_comment_prefix: Option<String>,
}

impl SyntaxDef {
    fn build(self) -> Result<minijinja::syntax::SyntaxConfig> {
        let mut b = minijinja::syntax::SyntaxConfig::builder();
        if let Some((s, e)) = self.block {
            b.block_delimiters(s, e);
        }
        if let Some((s, e)) = self.variable {
            b.variable_delimiters(s, e);
        }
        if let Some((s, e)) = self.comment {
            b.comment_delimiters(s, e);
        }
        if let Some(p) = self.line_statement_prefix {
            b.line_statement_prefix(p);
        }
        if let Some(p) = self.line_comment_prefix {
            b.line_comment_prefix(p);
        }
        Ok(b.build()?)
    }
}

/// `SYNTAX_PATH`, else a `syntax/` dir beside `templates_path`, else `./syntax`.
fn syntax_dir(templates_path: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("SYNTAX_PATH") {
        return p.into();
    }
    let sibling = templates_path
        .parent()
        .unwrap_or(Path::new("."))
        .join("syntax");
    if sibling.is_dir() {
        sibling
    } else {
        "./syntax".into()
    }
}

/// Loads every `*.yaml`/`*.yml`/`*.json` in `dir` as a named syntax (file stem
/// = name), plus the baked-in `default`. A missing dir is fine; a malformed or
/// unbuildable file is a startup error.
fn load_syntaxes(dir: &Path) -> Result<HashMap<String, minijinja::syntax::SyntaxConfig>> {
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => Some(entries),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("Cannot read {}", dir.display())),
    };
    for entry in entries.into_iter().flatten() {
        let path = entry?.path();
        let load = |def: Result<SyntaxDef, _>| -> Result<_> { def?.build() };
        let bytes = std::fs::read(&path)?;
        let syntax = match path.extension().and_then(|s| s.to_str()) {
            Some("json") => load(serde_json::from_slice(&bytes).map_err(anyhow::Error::from)),
            Some("yaml" | "yml") => {
                load(serde_saphyr::from_slice(&bytes).map_err(anyhow::Error::from))
            }
            _ => continue,
        }
        .with_context(|| format!("Invalid syntax definition {}", path.display()))?;
        // unwrap is safe: read_dir yields named files
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        out.insert(name, syntax);
    }
    // baked in, and not overridable
    out.insert("default".into(), Default::default());
    Ok(out)
}

/// Picks a template's syntax from a `templater: syntax="name"` magic line.
/// Unknown names fall back to `default` with a warning (the callback cannot fail).
fn configure_syntax(
    env: &mut minijinja::Environment<'static>,
    syntaxes: HashMap<String, minijinja::syntax::SyntaxConfig>,
) {
    env.set_syntax_callback(move |name, source| {
        let Some(wanted) = parse_magic(source).remove("syntax") else {
            return Default::default();
        };
        match syntaxes.get(&wanted) {
            Some(syntax) => syntax.clone(),
            None => {
                debug!("unknown syntax, using default"; "template" => name, "syntax" => &wanted);
                Default::default()
            }
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

#[test]
fn syntax_from_magic_line() {
    let dir = std::env::temp_dir().join("templater-syntax-test");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("alt.yaml"), "variable: ['${', '}']\n").unwrap();

    let mut env = minijinja::Environment::new();
    configure_syntax(&mut env, load_syntaxes(&dir).unwrap());
    let ctx = minijinja::context! { x => 42 };

    // magic line selects the loaded syntax
    assert_eq!(
        env.render_named_str("a.txt", "% templater: syntax=\"alt\"\n${x}", &ctx)
            .unwrap(),
        "% templater: syntax=\"alt\"\n42"
    );
    // no magic line, unknown name, and explicit "default" all keep jinja syntax
    for src in ["{{ x }}", "% templater: syntax=\"nope\"\n{{ x }}"] {
        assert!(
            env.render_named_str("a.txt", src, &ctx)
                .unwrap()
                .ends_with("42"),
            "should render with default syntax: {src:?}"
        );
    }
    // a missing syntax dir is not an error, and still has "default"
    assert!(
        load_syntaxes(Path::new("/nonexistent"))
            .unwrap()
            .contains_key("default")
    );

    std::fs::remove_dir_all(&dir).unwrap();
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
    pub fn new(
        templates_path: impl AsRef<Path>,
        assets_path: Option<impl AsRef<Path>>,
    ) -> Result<Self> {
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
        let syntax_dir = syntax_dir(templates_path.as_ref());
        debug!("loading syntaxes"; "syntax_path" => syntax_dir.display());
        configure_syntax(&mut jinja_env, load_syntaxes(&syntax_dir)?);
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

        Ok(State {
            jinja_env,
            reqwest_client,
            compile_semaphore,
        })
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
        let templated_file = TempFile::new_with_name_in(
            self.template.rendered_name(),
            self.dir.dir_path().to_owned(),
        )
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
