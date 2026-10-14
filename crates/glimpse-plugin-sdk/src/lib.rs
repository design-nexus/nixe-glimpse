//! Glimpse Plugin API v1. The host and SDK share these wire types.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{BufRead, Read, Write};
use std::path::PathBuf;
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub api_version: u32,
    pub id: u64,
    pub op: String,
    #[serde(default)]
    pub args: Value,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub text: String,
    #[serde(default)]
    pub page: Option<u32>,
    #[serde(default)]
    pub seconds: Option<f64>,
    #[serde(default)]
    pub line: Option<u32>,
}
pub fn validate_segments(segments: &[Segment]) -> Result<()> {
    if segments.len() > 128 || segments.iter().map(|s| s.text.len()).sum::<usize>() > 512 * 1024 {
        bail!("extracted content exceeds limits");
    }
    for s in segments {
        if s.page == Some(0) || s.line == Some(0) || s.seconds.is_some_and(|n| !n.is_finite() || n < 0.) {
            bail!("invalid segment location");
        }
    }
    Ok(())
}
/// Run a plugin over stdin/stdout; diagnostics belong on stderr. EOF shuts down.
pub fn serve(capabilities: &[&str], mut handle: impl FnMut(&Request) -> Result<Value>) -> Result<()> {
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        let n = input.by_ref().take(65537).read_until(b'\n', &mut bytes)?;
        if n == 0 {
            break;
        }
        if n > 65536 {
            bail!("request exceeds limit");
        }
        let request: Request = serde_json::from_slice(&bytes)?;
        let result = if request.api_version != API_VERSION {
            json!({"id":request.id,"error":"unsupported API version"})
        } else if request.op == "hello" {
            json!({"id":request.id,"api_version":API_VERSION,"capabilities":capabilities})
        } else if request.op == "shutdown" {
            break;
        } else {
            match handle(&request) {
                Ok(value) => json!({"id":request.id,"result":value}),
                Err(e) => json!({"id":request.id,"error":e.to_string()}),
            }
        };
        serde_json::to_writer(&mut output, &result)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    Ok(())
}

pub const API_VERSION: u32 = 1;
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    pub network: bool,
    pub selected_sources: bool,
    pub read: Vec<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: u32,
    pub exec: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Permissions,
    #[serde(default)]
    pub needs: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Copy { text: String },
    Open { path: PathBuf },
    Url { url: String },
    Activate { token: String },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hit {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    pub action: Action,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SettingsPage {
    pub rows: Vec<Setting>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Setting {
    Entry { key: String, title: String, value: String },
    Switch { key: String, title: String, value: bool },
    Choice { key: String, title: String, value: String, options: Vec<(String, String)> },
    Slider { key: String, title: String, value: f64, min: f64, max: f64, step: f64 },
}
