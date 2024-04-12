use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use anyhow::{bail, Context, Result};
use mime_guess::{mime, Mime, MimeGuess};
use nutype::nutype;
use reqwest::header;
use reqwest::Url;
use serde::Deserialize;
use tokio::io::{stdin, AsyncReadExt, BufReader};

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct RenderJob {
    pub template: TemplateRef,
    pub output: OutputRef,
    pub inputs: Vec<Input>,
}

#[nutype(derive(AsRef, From, FromStr, Clone, Debug, Deserialize, Eq, PartialEq))]
pub struct TemplateRef(String);

impl TemplateRef {
    const COMPILE_EXTENSIONS: [&'static str; 2] = ["tex", "mkiv"];

    pub fn should_compile(&self) -> bool {
        self.extension()
            .map(|ext| TemplateRef::COMPILE_EXTENSIONS.contains(&ext))
            .unwrap_or(false)
    }

    pub fn extension(&self) -> Option<&str> {
        Path::new(self.as_ref())
            .extension()
            .map(|ext| ext.to_str().unwrap())
    }

    pub fn mime_type(&self) -> Mime {
        self.extension()
            .and_then(|ext| MimeGuess::from_ext(ext).first())
            .unwrap_or(mime::TEXT_PLAIN)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(untagged)]
pub enum Input {
    FileRef(FileRef),
    Inline(HashMap<String, minijinja::Value>),
}

impl Input {
    pub async fn read_into_env(
        self,
        reqwest_client: &reqwest::Client,
    ) -> Result<HashMap<String, minijinja::Value>> {
        match self {
            Input::FileRef(fileref) => fileref.read_into_env(reqwest_client).await,
            Input::Inline(v) => Ok(v),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub enum OutputRef {
    File(FileRef),
}

impl FromStr for OutputRef {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        FileRef::from_str(s).map(Self::File)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(from = "&str")]
pub enum FileRef {
    Url(reqwest::Url),
    File(PathBuf),
}

impl FileRef {
    pub async fn read_into_env(
        &self,
        reqwest_client: &reqwest::Client,
    ) -> Result<HashMap<String, minijinja::Value>> {
        match self {
            FileRef::File(filename) => {
                if filename.as_os_str() == "-" {
                    let mut reader = BufReader::new(stdin());
                    let mut bytes = vec![];
                    reader
                        .read_to_end(&mut bytes)
                        .await
                        .context("Cannot read from stdin")?;
                    Ok(serde_json::from_slice(&bytes)?)
                } else {
                    let bytes = tokio::fs::read(filename).await.with_context(|| {
                        format!("Cannot open input file {}", filename.display())
                    })?;
                    let data = match filename.extension().and_then(|s| s.to_str()) {
                        Some("json") => serde_json::from_slice(&bytes)?,
                        Some("yaml") => serde_yaml::from_slice(&bytes)?,
                        _ => bail!("Unsupported input file {}", filename.display()),
                    };
                    Ok(data)
                }
            }
            FileRef::Url(url) => {
                let res = reqwest_client
                    .get(url.as_ref())
                    .send()
                    .await?
                    .error_for_status()?;

                let content_type = res
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<Mime>().ok())
                    .unwrap_or(mime::APPLICATION_JSON);

                let bytes = res.bytes().await?;

                // Note: it's not possible to use `mime::JSON`, because `mime::YAML` does not exist
                let data = match (content_type.type_(), content_type.subtype().as_str()) {
                    (mime::APPLICATION, "json") => serde_json::from_slice(&bytes)?,
                    (mime::APPLICATION, "yaml") => serde_yaml::from_slice(&bytes)?,
                    _ => bail!("Unsupported input file {}", content_type),
                };
                Ok(data)
            }
        }
    }
}

impl From<&str> for FileRef {
    fn from(value: &str) -> Self {
        Self::from_str(value).unwrap()
    }
}

impl FromStr for FileRef {
    type Err = anyhow::Error;

    /// This will parse the parameter and if it resembles a URL (i.e. reqwest can parse it) treat
    /// it as URL, if not as filename.
    fn from_str(str: &str) -> Result<Self> {
        Ok(match Url::parse(str) {
            Ok(url) => FileRef::Url(url),
            Err(_) => FileRef::File(Path::new(str).to_path_buf()),
        })
    }
}

#[cfg(test)]
mod test {
    use crate::*;
    use minijinja::Value;
    use std::str::FromStr;

    #[test]
    fn test_deserialization() {
        let sample = r#"{
            "template": "test.j2",
            "inputs": [{"test": "value"}],
            "output": "/test/file"
        }"#;
        let parsed: RenderJob = serde_json::from_str(sample).unwrap();
        let renderjob = RenderJob {
            template: TemplateRef::from("test.j2".to_string()),
            output: OutputRef::from_str("/test/file").unwrap(),
            inputs: vec![Input::Inline(HashMap::from([(
                "test".to_string(),
                Value::from_serializable(&"value"),
            )]))],
        };
        assert_eq!(parsed, renderjob);
    }
}
