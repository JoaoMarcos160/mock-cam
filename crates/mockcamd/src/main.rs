//! mockcamd — daemon que mantém a câmera virtual viva e aceita comandos via socket Unix.

use anyhow::{bail, Context, Result};
use mockcam_core::ipc::{self, Request, Response};
use mockcam_core::{Config, Engine};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("MOCKCAM_LOG").unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut cfg = Config::load();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => cfg.output.device = args.next().context("--device requer um caminho")?,
            "-h" | "--help" => {
                println!("uso: mockcamd [--device /dev/videoN]\n\nlog: MOCKCAM_LOG=debug  (GStreamer: GST_DEBUG=3)");
                return Ok(());
            }
            other => bail!("argumento desconhecido: {other}"),
        }
    }

    let path = ipc::socket_path();
    if path.exists() {
        if UnixStream::connect(&path).is_ok() {
            bail!("mockcamd já está rodando ({})", path.display());
        }
        std::fs::remove_file(&path).ok();
    }

    gst::init()?;
    let engine = Engine::new(cfg)?;
    engine.start()?;

    let listener = UnixListener::bind(&path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    tracing::info!("pronto — socket {}", path.display());

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let engine = engine.clone();
        std::thread::spawn(move || {
            if let Err(e) = serve(&engine, stream) {
                tracing::debug!("conexão encerrada: {e:#}");
            }
        });
    }
    Ok(())
}

fn serve(engine: &Engine, stream: UnixStream) -> Result<()> {
    let mut w = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (resp, exit) = match serde_json::from_str::<Request>(&line) {
            Ok(req) => engine.handle(req),
            Err(e) => (Response::err(format!("requisição inválida: {e}"), None), false),
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        w.write_all(out.as_bytes())?;
        if exit {
            tracing::info!("encerrando a pedido do cliente");
            engine.shutdown();
            std::fs::remove_file(ipc::socket_path()).ok();
            std::process::exit(0);
        }
    }
    Ok(())
}
