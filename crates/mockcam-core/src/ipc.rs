//! Protocolo de controle: JSON por linha sobre socket Unix.
//! Cada requisição recebe uma resposta com o status completo.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use crate::gpu::{Backend, GpuInfo};
use crate::source::CameraInfo;
use crate::{Effects, GpuMode, OutputCfg, SourceSpec};

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    SetSource { source: SourceSpec },
    SetEffects { effects: Effects },
    SetGpu { mode: GpuMode },
    SetOutput { output: OutputCfg },
    Freeze { on: bool },
    Pause,
    Resume,
    Seek { secs: f64 },
    Speed { rate: f64 },
    Record { secs: u32, then_loop: bool },
    Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Status {
    pub source: Option<SourceSpec>,
    /// "playing" | "paused" | "ended" | "error" | "starting"
    pub source_state: String,
    pub position: Option<f64>,
    pub duration: Option<f64>,
    pub speed: f64,
    pub frozen: bool,
    pub gpu_mode: GpuMode,
    pub backend: Backend,
    pub gpu: GpuInfo,
    pub gl_active: bool,
    pub gpu_note: Option<String>,
    pub effects: Effects,
    pub output: OutputCfg,
    pub device_ok: bool,
    pub output_error: Option<String>,
    pub out_fps: f64,
    pub in_fps: f64,
    pub recording: Option<f64>,
    pub last_clip: Option<String>,
    pub last_error: Option<String>,
    pub cameras: Vec<CameraInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Response {
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub status: Option<Status>,
}

impl Response {
    pub fn ok(status: Status) -> Self {
        Self { ok: true, error: None, status: Some(status) }
    }
    pub fn err(e: impl std::fmt::Display, status: Option<Status>) -> Self {
        Self { ok: false, error: Some(e.to_string()), status }
    }
}

pub fn socket_path() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("mockcam.sock"),
        None => std::env::temp_dir().join(format!("mockcam-{}.sock", std::env::var("USER").unwrap_or_default())),
    }
}

/// Envia uma requisição (uma conexão por chamada: simples e robusto).
pub fn request(req: &Request) -> Result<Response> {
    let path = socket_path();
    let mut s = UnixStream::connect(&path).with_context(|| format!("daemon não está rodando ({})", path.display()))?;
    s.set_read_timeout(Some(Duration::from_secs(15)))?;
    s.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    s.write_all(line.as_bytes())?;
    let mut resp = String::new();
    BufReader::new(&s).read_line(&mut resp)?;
    serde_json::from_str(&resp).context("resposta inválida do daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_json_shape() {
        let r = Request::SetSource { source: SourceSpec::File { path: "/a.mp4".into(), looped: true } };
        let s = serde_json::to_string(&r).unwrap();
        assert_eq!(s, r#"{"cmd":"set_source","source":{"kind":"file","path":"/a.mp4","looped":true}}"#);
        let back: Request = serde_json::from_str(r#"{"cmd":"set_source","source":{"kind":"file","path":"/a.mp4"}}"#).unwrap();
        assert!(matches!(back, Request::SetSource { source: SourceSpec::File { looped: true, .. } }));
        let st: Request = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert!(matches!(st, Request::Status));
    }
}
