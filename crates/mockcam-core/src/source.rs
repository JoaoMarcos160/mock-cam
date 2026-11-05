//! Pipelines de fonte (descartáveis). Todos terminam em um appsink que entrega
//! frames já escalados/convertidos para o formato de saída.

use anyhow::{bail, Context, Result};
use gst::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::Event;
use crate::gpu::Backend;
use crate::slot::FrameRouter;
use crate::util;
use crate::{OutputCfg, SourceSpec};

// ---- controle V4L2 (para devolver a webcam como estava) ----
const V4L2_CID_EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
const VIDIOC_G_CTRL: libc::c_ulong = 0xC008_561B;
const VIDIOC_S_CTRL: libc::c_ulong = 0xC008_561C;

#[repr(C)]
struct V4l2Control {
    id: u32,
    value: i32,
}

fn v4l2_ctrl(device: &str, set: Option<i32>) -> Option<i32> {
    use std::os::fd::AsRawFd;
    let f = std::fs::OpenOptions::new().read(true).write(true).open(device).ok()?;
    let mut c = V4l2Control { id: V4L2_CID_EXPOSURE_AUTO_PRIORITY, value: set.unwrap_or(0) };
    let req = if set.is_some() { VIDIOC_S_CTRL } else { VIDIOC_G_CTRL };
    // SAFETY: fd válido, struct com layout de v4l2_control.
    let rc = unsafe { libc::ioctl(f.as_raw_fd(), req, &mut c as *mut V4l2Control) };
    (rc == 0).then_some(c.value)
}

/// Nicks válidos de `videotestsrc pattern`.
pub const TEST_PATTERNS: &[&str] = &["smpte", "snow", "black", "white", "red", "green", "blue", "checkers-8", "circular", "ball", "smpte100", "bar", "pinwheel", "spokes", "gradient", "colors"];

pub struct Source {
    pub spec: SourceSpec,
    pipeline: gst::Pipeline,
    stop: Arc<AtomicBool>,
    watch: Option<JoinHandle<()>>,
    rate: Arc<Mutex<f64>>,
    looped: bool,
    pub paused: bool,
    /// (device, valor original) de exposure_dynamic_framerate a restaurar ao parar.
    restore: Option<(String, i32)>,
}

/// Trecho final comum: converte/escala para as caps de saída (com letterbox).
/// Obs.: escala via `vaapipostproc` foi avaliada, mas no gstreamer-vaapi 1.20 ele não
/// entrega memória de sistema a partir de frames de software; a GPU atua no decode.
fn tail(out: &OutputCfg, sync: bool, framerate: Option<u32>) -> String {
    let caps = out.caps_str();
    // Escala ANTES de converter: a conversão de cor roda sobre a imagem já reduzida
    // (~35% menos CPU em 1080p→720p).
    let conv = format!(
        "videoscale add-borders=true ! video/x-raw,width={},height={},pixel-aspect-ratio=1/1 ! videoconvert n-threads=2 ! {caps}",
        out.width, out.height
    );
    let sink = format!("appsink name=sink max-buffers=1 drop=true sync={sync} enable-last-sample=false");
    match framerate {
        Some(fr) => format!("{conv} ! imagefreeze ! video/x-raw,framerate={fr}/1 ! {sink}"),
        None => format!("{conv} ! {sink}"),
    }
}

/// Liga só os pads de vídeo do decodebin à fila `q` (áudio fica sem link e é descartado).
fn link_video_pads(p: &gst::Pipeline) -> Result<()> {
    let dec: gst::Element = util::by_name(p, "dec")?;
    let q: gst::Element = util::by_name(p, "q")?;
    dec.connect_pad_added(move |_, pad| {
        let caps = pad.current_caps().unwrap_or_else(|| pad.query_caps(None));
        let is_video = caps.structure(0).map(|s| s.name().as_str().starts_with("video/")).unwrap_or(false);
        if !is_video {
            return;
        }
        if let Some(sink) = q.static_pad("sink") {
            if !sink.is_linked() {
                if let Err(e) = pad.link(&sink) {
                    tracing::warn!("falha ao ligar pad de vídeo: {e:?}");
                }
            }
        }
    });
    Ok(())
}

impl Source {
    pub fn build(
        spec: &SourceSpec,
        out: &OutputCfg,
        _backend: Backend,
        router: Arc<FrameRouter>,
        events: Sender<Event>,
        gen: u64,
    ) -> Result<Source> {
        match spec {
            SourceSpec::Camera { device, fixed_fps } => {
                // UVC: resoluções altas a 30 fps normalmente só em MJPEG; tenta MJPEG primeiro.
                let decode = format!("decodebin name=dec queue name=q max-size-buffers=2 leaky=downstream ! {}", tail(out, false, None));
                let attempts = [
                    format!("v4l2src name=src ! image/jpeg,width={},height={} ! {decode}", out.width, out.height),
                    format!("v4l2src name=src ! image/jpeg ! {decode}"),
                    format!("v4l2src name=src ! {decode}"),
                ];
                let original = v4l2_ctrl(device, None);
                let mut last = None;
                for desc in &attempts {
                    let r = Self::start(spec, desc, router.clone(), events.clone(), gen, |p| {
                        let src: gst::Element = util::by_name(p, "src")?;
                        src.set_property("device", device);
                        let v = if *fixed_fps { 0 } else { 1 };
                        src.set_property_from_str("extra-controls", &format!("c,exposure_dynamic_framerate={v}"));
                        Ok(())
                    });
                    match r {
                        Ok(mut s) => {
                            s.restore = original.map(|v| (device.clone(), v));
                            return Ok(s);
                        }
                        Err(e) => {
                            tracing::debug!("câmera: tentativa falhou: {e:#}");
                            last = Some(e);
                        }
                    }
                }
                Err(last.unwrap()).context(format!("não foi possível abrir a câmera {device}"))
            }
            SourceSpec::File { path, .. } => {
                anyhow::ensure!(std::path::Path::new(path).is_file(), "arquivo não encontrado: {path}");
                let desc = format!(
                    "filesrc name=fsrc ! decodebin name=dec queue name=q max-size-buffers=3 ! {}",
                    tail(out, true, None)
                );
                Self::start(spec, &desc, router, events, gen, |p| {
                    util::by_name::<gst::Element>(p, "fsrc")?.set_property("location", path);
                    Ok(())
                })
            }
            SourceSpec::Image { path } => {
                anyhow::ensure!(std::path::Path::new(path).is_file(), "arquivo não encontrado: {path}");
                let desc = format!("filesrc name=fsrc ! decodebin name=dec queue name=q ! {}", tail(out, true, Some(5)));
                Self::start(spec, &desc, router, events, gen, |p| {
                    util::by_name::<gst::Element>(p, "fsrc")?.set_property("location", path);
                    Ok(())
                })
            }
            SourceSpec::Test { pattern } => {
                let desc = format!(
                    "videotestsrc name=src is-live=true ! video/x-raw,framerate={}/1 ! {}",
                    out.fps,
                    tail(out, false, None)
                );
                if !TEST_PATTERNS.contains(&pattern.as_str()) {
                    bail!("padrão de teste desconhecido: {pattern} (use: {})", TEST_PATTERNS.join(", "));
                }
                Self::start(spec, &desc, router, events, gen, |p| {
                    util::by_name::<gst::Element>(p, "src")?.set_property_from_str("pattern", pattern);
                    Ok(())
                })
            }
            SourceSpec::Black => {
                let desc = format!("videotestsrc is-live=true pattern=black ! video/x-raw,framerate=5/1 ! {}", tail(out, false, None));
                Self::start(spec, &desc, router, events, gen, |_| Ok(()))
            }
        }
    }

    fn start(
        spec: &SourceSpec,
        desc: &str,
        router: Arc<FrameRouter>,
        events: Sender<Event>,
        gen: u64,
        configure: impl FnOnce(&gst::Pipeline) -> Result<()>,
    ) -> Result<Source> {
        let p = util::launch(desc)?;
        configure(&p)?;
        if p.by_name("dec").is_some() {
            link_video_pads(&p)?;
        }

        let got = Arc::new(AtomicBool::new(false));
        let sink: gst_app::AppSink = util::by_name(&p, "sink")?;
        {
            let got = got.clone();
            util::on_buffer(&sink, move |buf| {
                got.store(true, Ordering::Relaxed);
                router.push_raw(gen, buf);
            });
        }

        let looped = matches!(spec, SourceSpec::File { looped: true, .. });
        let fail = |p: &gst::Pipeline, e: anyhow::Error| -> anyhow::Error {
            let _ = p.set_state(gst::State::Null);
            e
        };

        if spec.is_seekable() {
            // Preroll → seek inicial (com flag SEGMENT para loop sem emenda) → play.
            p.set_state(gst::State::Paused).map_err(|e| fail(&p, e.into()))?;
            let (res, _, _) = p.state(gst::ClockTime::from_seconds(10));
            if res.is_err() {
                let msg = p
                    .bus()
                    .and_then(|b| b.pop_filtered(&[gst::MessageType::Error]))
                    .and_then(|m| match m.view() {
                        gst::MessageView::Error(e) => Some(util::error_text(e)),
                        _ => None,
                    })
                    .unwrap_or_else(|| "falha no preroll".into());
                return Err(fail(&p, anyhow::anyhow!(msg)));
            }
            let mut flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
            if looped {
                flags |= gst::SeekFlags::SEGMENT;
            }
            p.seek(1.0, flags, gst::SeekType::Set, gst::ClockTime::ZERO, gst::SeekType::None, gst::ClockTime::NONE)
                .map_err(|e| fail(&p, e.into()))?;
        }
        p.set_state(gst::State::Playing).map_err(|e| fail(&p, e.into()))?;
        util::wait_first_frame(&p, &got, Duration::from_secs(4)).map_err(|e| fail(&p, e))?;

        let stop = Arc::new(AtomicBool::new(false));
        let rate = Arc::new(Mutex::new(1.0f64));
        let watch = {
            let pw = p.downgrade();
            let rate = rate.clone();
            util::spawn_bus_watch(&p, stop.clone(), move |msg| match msg.view() {
                gst::MessageView::SegmentDone(_) => {
                    // Loop sem emenda: novo seek de segmento SEM flush.
                    if let Some(p) = pw.upgrade() {
                        let r = *rate.lock().unwrap();
                        let _ = p.seek(
                            r,
                            gst::SeekFlags::SEGMENT,
                            gst::SeekType::Set,
                            gst::ClockTime::ZERO,
                            gst::SeekType::None,
                            gst::ClockTime::NONE,
                        );
                    }
                }
                gst::MessageView::Eos(_) => {
                    let _ = events.send(Event::SourceEos { gen });
                }
                gst::MessageView::Error(e) => {
                    let _ = events.send(Event::SourceError { gen, msg: util::error_text(e) });
                }
                _ => {}
            })
        };

        Ok(Source { spec: spec.clone(), pipeline: p, stop, watch, rate, looped, paused: false, restore: None })
    }

    pub fn uses_device(&self, dev: &str) -> bool {
        matches!(&self.spec, SourceSpec::Camera { device, .. } if device == dev)
    }

    pub fn stop(self) {
        std::thread::spawn(move || self.stop_blocking());
    }

    pub fn stop_blocking(self) {
        util::stop_blocking(self.pipeline, self.stop, self.watch);
        if let Some((dev, v)) = self.restore {
            v4l2_ctrl(&dev, Some(v));
        }
    }

    pub fn position(&self) -> Option<f64> {
        self.pipeline.query_position::<gst::ClockTime>().map(|t| t.nseconds() as f64 / 1e9)
    }

    pub fn duration(&self) -> Option<f64> {
        self.pipeline.query_duration::<gst::ClockTime>().map(|t| t.nseconds() as f64 / 1e9)
    }

    pub fn rate(&self) -> f64 {
        *self.rate.lock().unwrap()
    }

    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        let st = if paused { gst::State::Paused } else { gst::State::Playing };
        self.pipeline.set_state(st)?;
        self.paused = paused;
        Ok(())
    }

    fn seek_to(&self, rate: f64, pos: gst::ClockTime) -> Result<()> {
        let mut flags = gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE;
        if self.looped {
            flags |= gst::SeekFlags::SEGMENT;
        }
        self.pipeline
            .seek(rate, flags, gst::SeekType::Set, pos, gst::SeekType::None, gst::ClockTime::NONE)?;
        Ok(())
    }

    pub fn seek(&self, secs: f64) -> Result<()> {
        anyhow::ensure!(self.spec.is_seekable(), "a fonte atual não suporta seek");
        let pos = gst::ClockTime::from_nseconds((secs.max(0.0) * 1e9) as u64);
        self.seek_to(self.rate(), pos)
    }

    pub fn set_rate(&self, rate: f64) -> Result<()> {
        anyhow::ensure!(self.spec.is_seekable(), "a fonte atual não suporta velocidade");
        let rate = rate.clamp(0.25, 4.0);
        let pos = self.pipeline.query_position::<gst::ClockTime>().unwrap_or(gst::ClockTime::ZERO);
        self.seek_to(rate, pos)?;
        *self.rate.lock().unwrap() = rate;
        Ok(())
    }
}

/// Câmeras de captura reais (exclui o próprio dispositivo de saída).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct CameraInfo {
    pub device: String,
    pub name: String,
}

pub fn list_cameras(exclude: &str) -> Vec<CameraInfo> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/video4linux") else { return out };
    for e in rd.flatten() {
        let node = e.file_name().to_string_lossy().to_string();
        let dev = format!("/dev/{node}");
        if dev == exclude {
            continue;
        }
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).map(|s| s.trim().to_string()).unwrap_or_default();
        // index 0 = nó de captura principal (os demais costumam ser metadata).
        if read("index") != "0" {
            continue;
        }
        let name = read("name");
        if name.to_lowercase().contains("mockcam") {
            continue;
        }
        out.push(CameraInfo { device: dev, name });
    }
    out.sort_by(|a, b| a.device.cmp(&b.device));
    out
}
