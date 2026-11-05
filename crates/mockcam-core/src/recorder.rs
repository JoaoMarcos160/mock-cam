//! Grava N segundos dos frames crus (pré-efeitos) em /dev/shm, para virar loop.
//! Lê do slot `raw` no próprio ritmo, então nunca interfere na fonte nem na saída.

use anyhow::Result;
use gst::prelude::*;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::Event;
use crate::gpu::{self, Backend};
use crate::slot::FrameRouter;
use crate::util;
use crate::OutputCfg;

pub fn clips_dir() -> PathBuf {
    let shm = PathBuf::from("/dev/shm");
    let base = if shm.is_dir() { shm } else { std::env::temp_dir() };
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    base.join(format!("mockcam-{user}"))
}

/// Mantém só os clipes mais recentes (RAM é preciosa).
fn prune(dir: &PathBuf, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut files: Vec<_> = rd.flatten().filter(|e| e.path().extension().is_some_and(|x| x == "mp4")).collect();
    files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
    let excess = files.len().saturating_sub(keep);
    for f in files.into_iter().take(excess) {
        let _ = std::fs::remove_file(f.path());
    }
}

pub fn spawn(
    secs: u32,
    then_loop: bool,
    out: OutputCfg,
    backend: Backend,
    router: Arc<FrameRouter>,
    progress: Arc<Mutex<Option<f64>>>,
    events: Sender<Event>,
) -> Result<()> {
    let dir = clips_dir();
    std::fs::create_dir_all(&dir)?;
    prune(&dir, 4);
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    let path = dir.join(format!("clip-{ts}.mp4"));

    // h264parse (plugins-bad) converte byte-stream→avc; x264enc já negocia avc sem ele.
    let parse = if gst::ElementFactory::find("h264parse").is_some() { "h264parse !" } else { "" };
    let backend = if parse.is_empty() { Backend::Cpu } else { backend };
    let enc = gpu::h264_encoder(backend);
    let gpu_enc = !enc.starts_with("x264enc");
    let desc = format!("appsrc name=src is-live=true format=time ! videoconvert ! {enc} ! {parse} mp4mux ! filesink name=fs");
    let p = util::launch(&desc)?;
    util::by_name::<gst::Element>(&p, "fs")?.set_property("location", path.to_string_lossy().to_string());
    let src: gst_app::AppSrc = util::by_name(&p, "src")?;
    src.set_caps(Some(&gst::Caps::from_str(&format!("{},framerate={}/1", out.caps_str(), out.fps))?));
    p.set_state(gst::State::Playing)?;

    *progress.lock().unwrap() = Some(0.0);
    std::thread::Builder::new().name("mockcam-rec".into()).spawn(move || {
        let result = (|| -> Result<()> {
            let period = Duration::from_nanos(1_000_000_000 / out.fps as u64);
            let total = (secs.max(1) * out.fps) as u64;
            let t0 = Instant::now();
            let expected = util::black_frame(&out).size();
            for n in 0..total {
                let target = t0 + Duration::from_nanos(n * period.as_nanos() as u64);
                if let Some(d) = target.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
                let Some(b) = router.raw.get().filter(|b| b.size() >= expected) else { continue };
                let mut buf = b.copy();
                {
                    let bm = buf.get_mut().unwrap();
                    bm.set_pts(gst::ClockTime::from_nseconds(n * period.as_nanos() as u64));
                    bm.set_duration(gst::ClockTime::from_nseconds(period.as_nanos() as u64));
                }
                src.push_buffer(buf).map_err(|e| anyhow::anyhow!("gravação: {e:?}"))?;
                *progress.lock().unwrap() = Some((n + 1) as f64 / total as f64);
            }
            src.end_of_stream().map_err(|e| anyhow::anyhow!("gravação: {e:?}"))?;
            let bus = p.bus().unwrap();
            match bus.timed_pop_filtered(gst::ClockTime::from_seconds(15), &[gst::MessageType::Eos, gst::MessageType::Error]) {
                Some(m) => {
                    if let gst::MessageView::Error(e) = m.view() {
                        anyhow::bail!(util::error_text(e));
                    }
                }
                None => anyhow::bail!("timeout finalizando o MP4"),
            }
            Ok(())
        })();
        let _ = p.set_state(gst::State::Null);
        *progress.lock().unwrap() = None;
        let _ = match result {
            Ok(()) => events.send(Event::RecordDone { path: path.to_string_lossy().to_string(), then_loop }),
            Err(e) => events.send(Event::RecordFailed { msg: format!("{e:#}"), secs, then_loop, gpu_enc }),
        };
    })?;
    Ok(())
}
