//! Saída para o dispositivo virtual: thread com relógio próprio a fps constante.
//! Nunca espera a fonte: se não chegou frame novo, repete o último.
//!
//! Escreve direto no v4l2loopback (`VIDIOC_S_FMT` + um `write()` por frame) em vez
//! de usar o `v4l2sink`: no GStreamer 1.20 o v4l2sink dá segfault ao desligar com
//! v4l2loopback, e o caminho direto ainda evita um pipeline inteiro.

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::slot::FrameRouter;
use crate::util;
use crate::{OutputCfg, PixelFormat};

#[derive(Default)]
pub struct OutStats {
    out_fps_bits: AtomicU64,
    in_fps_bits: AtomicU64,
    pub frozen: AtomicBool,
    pub error: Mutex<Option<String>>,
    /// true quando escrevendo em um dispositivo real.
    pub device_ok: AtomicBool,
}

impl OutStats {
    pub fn out_fps(&self) -> f64 {
        f64::from_bits(self.out_fps_bits.load(Ordering::Relaxed))
    }
    pub fn in_fps(&self) -> f64 {
        f64::from_bits(self.in_fps_bits.load(Ordering::Relaxed))
    }
}

// ---- V4L2 mínimo (linux/videodev2.h) ----

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format`: type (u32) + union de 200 bytes alinhada a 8 → 208 bytes.
#[repr(C)]
struct V4l2Format {
    type_: u32,
    _pad: u32,
    pix: V4l2PixFormat,
    _rest: [u8; 200 - std::mem::size_of::<V4l2PixFormat>()],
}

const _: () = assert!(std::mem::size_of::<V4l2Format>() == 208);

const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SMPTE170M: u32 = 1; // BT.601, o que o v4l2loopback anuncia
/// _IOWR('V', 5, struct v4l2_format)
const VIDIOC_S_FMT: libc::c_ulong = 0xC0D0_5605;
/// _IOW('V', 18, int)
const VIDIOC_STREAMON: libc::c_ulong = 0x4004_5612;

/// Registra este fd como escritor no v4l2loopback. Essencial: no 0.12.x, um escritor
/// que só usa write() nunca é marcado como WRITER, e ao fechar o driver não volta a
/// anunciar saída — o dispositivo fica travado (S_FMT → EINVAL) até recarregar o módulo.
fn stream_on(f: &File) -> std::io::Result<()> {
    let mut t: libc::c_int = V4L2_BUF_TYPE_VIDEO_OUTPUT as libc::c_int;
    // SAFETY: fd válido; o argumento é um int com o tipo de buffer.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), VIDIOC_STREAMON, &mut t as *mut libc::c_int) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    (s[0] as u32) | (s[1] as u32) << 8 | (s[2] as u32) << 16 | (s[3] as u32) << 24
}

fn open_device(cfg: &OutputCfg, frame_size: usize) -> Result<File> {
    match try_open_device(cfg, frame_size) {
        Ok(f) => Ok(f),
        Err(first) => {
            // Recuperação de um dispositivo travado por um escritor anterior (ver stream_on):
            // STREAMON + close faz o driver voltar a anunciar saída. Depois tenta de novo.
            tracing::warn!("{first:#}; tentando recuperar o dispositivo");
            if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&cfg.device) {
                let _ = stream_on(&f);
            }
            try_open_device(cfg, frame_size).map_err(|e| e.context(format!("recuperação falhou (antes: {first:#})")))
        }
    }
}

fn try_open_device(cfg: &OutputCfg, frame_size: usize) -> Result<File> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(&cfg.device)
        .with_context(|| format!("abrindo {}", cfg.device))?;
    let (pixelformat, bpl) = match cfg.format {
        PixelFormat::Yuy2 => (fourcc(b"YUYV"), cfg.width * 2),
        PixelFormat::I420 => (fourcc(b"YU12"), cfg.width),
        PixelFormat::Nv12 => (fourcc(b"NV12"), cfg.width),
    };
    let mut fmt = V4l2Format {
        type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
        _pad: 0,
        pix: V4l2PixFormat {
            width: cfg.width,
            height: cfg.height,
            pixelformat,
            field: V4L2_FIELD_NONE,
            bytesperline: bpl,
            sizeimage: frame_size as u32,
            colorspace: V4L2_COLORSPACE_SMPTE170M,
            ..Default::default()
        },
        _rest: [0; 200 - std::mem::size_of::<V4l2PixFormat>()],
    };
    // SAFETY: fd válido e struct com o layout exato de v4l2_format.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), VIDIOC_S_FMT, &mut fmt as *mut V4l2Format) };
    if r < 0 {
        bail!(
            "VIDIOC_S_FMT em {} falhou: {} (o formato pode estar travado por um app usando a câmera)",
            cfg.device,
            std::io::Error::last_os_error()
        );
    }
    if fmt.pix.width != cfg.width || fmt.pix.height != cfg.height || fmt.pix.pixelformat != pixelformat {
        bail!(
            "{} recusou {}x{} {:?} (ficou {}x{}); feche os apps que usam a câmera e tente de novo",
            cfg.device,
            cfg.width,
            cfg.height,
            cfg.format,
            fmt.pix.width,
            fmt.pix.height
        );
    }
    stream_on(&f).with_context(|| format!("VIDIOC_STREAMON em {}", cfg.device))?;
    Ok(f)
}

pub struct Output {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Output {
    pub fn start(cfg: &OutputCfg, router: Arc<FrameRouter>, stats: Arc<OutStats>) -> Result<Output> {
        let dev = try_attach(cfg, &stats);

        let stop = Arc::new(AtomicBool::new(false));
        let cfg = cfg.clone();
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("mockcam-out".into())
                .spawn(move || run(cfg, dev, router, stats, stop))?
        };
        Ok(Output { stop, thread: Some(thread) })
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Abre o dispositivo e atualiza o status (erro visível no painel/CLI).
fn try_attach(cfg: &OutputCfg, stats: &OutStats) -> Option<File> {
    let frame_size = util::black_frame(cfg).size();
    let dev = if std::path::Path::new(&cfg.device).exists() {
        match open_device(cfg, frame_size) {
            Ok(f) => {
                *stats.error.lock().unwrap() = None;
                Some(f)
            }
            Err(e) => {
                *stats.error.lock().unwrap() = Some(format!("{e:#}"));
                None
            }
        }
    } else {
        *stats.error.lock().unwrap() = Some(format!(
            "dispositivo {} não existe — rode `sudo mockcam-setup` (v4l2loopback). Saída descartada.",
            cfg.device
        ));
        None
    };
    stats.device_ok.store(dev.is_some(), Ordering::Relaxed);
    dev
}

fn run(cfg: OutputCfg, mut dev: Option<File>, router: Arc<FrameRouter>, stats: Arc<OutStats>, stop: Arc<AtomicBool>) {
    let period = Duration::from_nanos(1_000_000_000 / cfg.fps as u64);
    let black = util::black_frame(&cfg);
    let expected = black.size();
    let mut held: Option<gst::Buffer> = None;
    let t0 = Instant::now();
    let mut n: u64 = 0;
    let (mut win_start, mut win_out, mut win_in) = (Instant::now(), 0u64, router.raw.count());
    let mut last_err = Instant::now() - Duration::from_secs(10);
    let mut last_attach = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Relógio absoluto (sem drift); se atrasou demais, ressincroniza em vez de "correr atrás".
        let target = t0 + Duration::from_nanos(n * period.as_nanos() as u64);
        let now = Instant::now();
        if target > now {
            std::thread::sleep(target - now);
        } else if now - target > period * 3 {
            n = ((now - t0).as_nanos() / period.as_nanos()) as u64;
        }

        if !stats.frozen.load(Ordering::Relaxed) {
            if let Some(b) = router.out.get() {
                // Frame de tamanho errado (troca de caps em andamento) é ignorado.
                if b.size() >= expected {
                    held = Some(b);
                }
            }
        }

        // Sem dispositivo (ausente, travado ou erro de escrita): tenta reabrir a cada 2 s.
        if dev.is_none() && last_attach.elapsed() >= Duration::from_secs(2) {
            last_attach = Instant::now();
            dev = try_attach(&cfg, &stats);
            if dev.is_some() {
                tracing::info!("saída reconectada a {}", cfg.device);
            }
        }

        let mut write_failed = false;
        if let Some(f) = dev.as_mut() {
            let base = held.as_ref().unwrap_or(&black);
            if let Ok(map) = base.map_readable() {
                // Um write() = um frame no v4l2loopback.
                if let Err(e) = f.write_all(&map[..expected]) {
                    write_failed = true;
                    if last_err.elapsed() > Duration::from_secs(5) {
                        tracing::warn!("escrita na saída falhou: {e}");
                        *stats.error.lock().unwrap() = Some(format!("escrita em {} falhou: {e}", cfg.device));
                        last_err = Instant::now();
                    }
                }
            }
        }
        if write_failed {
            // Fecha e deixa a lógica acima reabrir (recupera de estados ruins do driver).
            dev = None;
            stats.device_ok.store(false, Ordering::Relaxed);
            last_attach = Instant::now();
        }
        n += 1;
        win_out += 1;

        let el = win_start.elapsed();
        if el >= Duration::from_secs(1) {
            let secs = el.as_secs_f64();
            let in_now = router.raw.count();
            stats.out_fps_bits.store((win_out as f64 / secs).to_bits(), Ordering::Relaxed);
            stats.in_fps_bits.store(((in_now - win_in) as f64 / secs).to_bits(), Ordering::Relaxed);
            win_start = Instant::now();
            win_out = 0;
            win_in = in_now;
        }
    }
    drop(dev.take());
}
