//! mockcam — CLI de controle do daemon.

use anyhow::{bail, Context, Result};
use mockcam_core::ipc::{self, Request, Status};
use mockcam_core::{GpuMode, Rotation, SourceSpec};

const HELP: &str = "\
uso: mockcam <comando>

  status                       estado atual
  cameras                      lista câmeras reais
  camera [/dev/videoN] [--dynamic-fps]
                               fonte = câmera ao vivo (padrão: fps fixo mesmo no escuro)
  file <arquivo> [--once]      fonte = vídeo (loop por padrão)
  image <arquivo>              fonte = imagem estática
  test [padrão]                fonte = padrão de teste (smpte, ball, snow...)
  black                        fonte = preto
  record <seg> [--loop]        grava N s da fonte atual (--loop: passa a tocar em loop)
  pause | resume               pausa/retoma o vídeo
  seek <seg>                   posiciona o vídeo
  speed <taxa>                 velocidade 0.25..4
  freeze on|off                congela o frame de saída
  gpu off|auto|intel|nvidia    aceleração por hardware
  mirror on|off                espelhar
  rotate 0|90|180|270          rotacionar
  text \"<texto>\"              overlay de texto (vazio remove)
  text-mirror on|off           texto invertido (legível no seu preview espelhado)
  output <W> <H> <FPS> [yuy2|i420|nv12]
  shutdown                     encerra o daemon";

fn main() {
    if let Err(e) = run() {
        eprintln!("erro: {e:#}");
        std::process::exit(1);
    }
}

fn status() -> Result<Status> {
    ipc::request(&Request::Status)?.status.context("sem status")
}

fn on_off(v: Option<&String>) -> Result<bool> {
    match v.map(|s| s.as_str()) {
        Some("on") | Some("1") | Some("true") => Ok(true),
        Some("off") | Some("0") | Some("false") => Ok(false),
        _ => bail!("use on|off"),
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("status");
    let arg = |i: usize| args.get(i).with_context(|| format!("faltou argumento\n\n{HELP}"));

    let req = match cmd {
        "status" => Request::Status,
        "cameras" => {
            for c in status()?.cameras {
                println!("{}\t{}", c.device, c.name);
            }
            return Ok(());
        }
        "camera" => {
            let device = match args.get(1).filter(|a| !a.starts_with("--")) {
                Some(d) => d.clone(),
                None => status()?.cameras.first().map(|c| c.device.clone()).context("nenhuma câmera encontrada")?,
            };
            let fixed_fps = !args.iter().any(|a| a == "--dynamic-fps");
            Request::SetSource { source: SourceSpec::Camera { device, fixed_fps } }
        }
        "file" => {
            let path = std::fs::canonicalize(arg(1)?)?.to_string_lossy().to_string();
            let looped = !args.iter().any(|a| a == "--once");
            Request::SetSource { source: SourceSpec::File { path, looped } }
        }
        "image" => {
            let path = std::fs::canonicalize(arg(1)?)?.to_string_lossy().to_string();
            Request::SetSource { source: SourceSpec::Image { path } }
        }
        "test" => Request::SetSource { source: SourceSpec::Test { pattern: args.get(1).cloned().unwrap_or("smpte".into()) } },
        "black" => Request::SetSource { source: SourceSpec::Black },
        "record" => Request::Record { secs: arg(1)?.parse()?, then_loop: args.iter().any(|a| a == "--loop") },
        "pause" => Request::Pause,
        "resume" => Request::Resume,
        "seek" => Request::Seek { secs: arg(1)?.parse()? },
        "speed" => Request::Speed { rate: arg(1)?.parse()? },
        "freeze" => Request::Freeze { on: on_off(args.get(1))? },
        "gpu" => Request::SetGpu {
            mode: match arg(1)?.as_str() {
                "off" => GpuMode::Off,
                "auto" => GpuMode::Auto,
                "intel" => GpuMode::Intel,
                "nvidia" => GpuMode::Nvidia,
                o => bail!("modo de GPU inválido: {o}"),
            },
        },
        "mirror" | "rotate" | "text" | "text-mirror" => {
            let mut fx = status()?.effects;
            match cmd {
                "mirror" => fx.mirror = on_off(args.get(1))?,
                "text-mirror" => fx.text_mirrored = on_off(args.get(1))?,
                "rotate" => {
                    fx.rotation = match arg(1)?.as_str() {
                        "0" => Rotation::R0,
                        "90" => Rotation::R90,
                        "180" => Rotation::R180,
                        "270" => Rotation::R270,
                        o => bail!("rotação inválida: {o}"),
                    }
                }
                _ => fx.text = args.get(1).cloned().unwrap_or_default(),
            }
            Request::SetEffects { effects: fx }
        }
        "output" => {
            let mut o = status()?.output;
            o.width = arg(1)?.parse()?;
            o.height = arg(2)?.parse()?;
            o.fps = arg(3)?.parse()?;
            if let Some(f) = args.get(4) {
                o.format = match f.to_lowercase().as_str() {
                    "yuy2" => mockcam_core::PixelFormat::Yuy2,
                    "i420" => mockcam_core::PixelFormat::I420,
                    "nv12" => mockcam_core::PixelFormat::Nv12,
                    o => bail!("formato inválido: {o}"),
                };
            }
            Request::SetOutput { output: o }
        }
        "shutdown" => Request::Shutdown,
        "-h" | "--help" | "help" => {
            println!("{HELP}");
            return Ok(());
        }
        o => bail!("comando desconhecido: {o}\n\n{HELP}"),
    };

    let resp = ipc::request(&req)?;
    if let Some(st) = &resp.status {
        print_status(st);
    }
    if !resp.ok {
        bail!(resp.error.unwrap_or_default());
    }
    Ok(())
}

fn print_status(s: &Status) {
    let src = s.source.as_ref().map(|s| s.label()).unwrap_or_else(|| "—".into());
    println!("fonte:   {src} [{}]", s.source_state);
    if let (Some(p), Some(d)) = (s.position, s.duration) {
        println!("posição: {p:.1}s / {d:.1}s  (vel {:.2}x)", s.speed);
    }
    println!(
        "saída:   {} {}x{}@{} {:?}  out {:.1} fps | in {:.1} fps{}",
        s.output.device,
        s.output.width,
        s.output.height,
        s.output.fps,
        s.output.format,
        s.out_fps,
        s.in_fps,
        if s.frozen { " [CONGELADO]" } else { "" }
    );
    println!(
        "gpu:     modo {:?} → {:?} (vaapi={} nvidia={} gl={}){}",
        s.gpu_mode,
        s.backend,
        s.gpu.vaapi,
        s.gpu.nvidia,
        s.gpu.gl,
        if s.gl_active { " [filtro GL ativo]" } else { "" }
    );
    if let Some(r) = s.recording {
        println!("gravando: {:.0}%", r * 100.0);
    }
    for (k, v) in [("aviso gpu", &s.gpu_note), ("saída", &s.output_error), ("erro", &s.last_error)] {
        if let Some(v) = v {
            println!("{k}: {v}");
        }
    }
}
