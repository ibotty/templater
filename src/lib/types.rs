use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use mime_guess::{mime, Mime, MimeGuess};
use nutype::nutype;
use reqwest::Url;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct RenderJob {
    pub data: HashMap<String, minijinja::Value>,
    pub template: TemplateRef,
    pub output: OutputRef,
}

#[nutype(derive(AsRef, From, FromStr, Clone, Debug, Deserialize))]
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

#[nutype(derive(AsRef, From, FromStr, Clone, Debug, Deserialize))]
pub struct InputRef(PathBuf);

#[nutype(derive(AsRef, Clone, FromStr, Debug, Deserialize))]
pub struct OutputRef(FileRef);

#[derive(Clone, Debug, Deserialize)]
#[serde(from = "&str")]
pub enum FileRef {
    Url(reqwest::Url),
    File(PathBuf),
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
    fn from_str(str: &str) -> anyhow::Result<Self> {
        Ok(match Url::parse(str) {
            Ok(url) => FileRef::Url(url),
            Err(_) => FileRef::File(Path::new(str).to_path_buf()),
        })
    }
}

//impl FromStr for OutputRef {
//    type Err = anyhow::Error;
//
//    fn from_str(str: &str) -> anyhow::Result<Self> {
//        FileRef::from_str(str).map(Self::new)
//    }
//}
