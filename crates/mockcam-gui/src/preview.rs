//! Preview lendo o próprio dispositivo virtual: mostra exatamente o que os apps veem,
//! sem trafegar frames por IPC. Reduzido para 640 px de largura em RGBA.

use anyhow::{anyhow, Context, Result};
use eframe::egui;
use gst::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct Frame {
    pub w: usize,
    pub h: usize,
    pub rgba: Vec<u8>,
}

#[derive(Clone)]
pub struct Preview {
    pub frame: Arc<Mutex<Option<Frame>>>,
    pub enabled: Arc<AtomicBool>,
    pub device: Arc<Mutex<String>>,
    /// Resolução de saída (largura, altura) para manter a proporção do preview.
    pub out_size: Arc<Mutex<(u32, u32)>>,
    pub error: Arc<Mutex<Option<String>>>,
}

impl Preview {
    pub fn new(ctx: egui::Context) -> Preview {
        let p = Preview {
            frame: Default::default(),
            enabled: Arc::new(AtomicBool::new(true)),
            device: Arc::new(Mutex::new("/dev/video10".into())),
            out_size: Arc::new(Mutex::new((1280, 720))),
            error: Default::default(),
        };
        let me = p.clone();
        std::thread::Builder::new()
            .name("mockcam-preview".into())
            .spawn(move || loop {
                if !me.enabled.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(300));
                    continue;
                }
                let dev = me.device.lock().unwrap().clone();
                let size = *me.out_size.lock().unwrap();
                let r = me.run_once(&dev, size, &ctx);
                *me.error.lock().unwrap() = r.err().map(|e| format!("{e:#}"));
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(1));
            })
            .expect("thread de preview");
        p
    }

    /// Roda até erro, troca de dispositivo ou preview desligado.
    fn run_once(&self, dev: &str, size: (u32, u32), ctx: &egui::Context) -> Result<()> {
        if !std::path::Path::new(dev).exists() {
            return Err(anyhow!("{dev} não existe (rode o mockcam-setup)"));
        }
        // Altura explícita: o v4l2loopback não informa pixel-aspect e, só com a largura,
        // o videoscale escolheria 640x720 com pixel 2:1 (preview esticado).
        let h = ((640 * size.1 / size.0.max(1)) / 2 * 2).max(2);
        let pipeline = gst::parse::launch(&format!(
            "v4l2src name=src ! videoconvert ! videoscale ! \
             video/x-raw,format=RGBA,width=640,height={h} ! \
             appsink name=sink max-buffers=1 drop=true sync=false enable-last-sample=false"
        ))?
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow!("pipeline"))?;
        pipeline.by_name("src").context("src")?.set_property("device", dev);
        let sink = pipeline
            .by_name("sink")
            .context("sink")?
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("appsink"))?;

        let frame = self.frame.clone();
        let ctx2 = ctx.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let (Some(buf), Some(caps)) = (sample.buffer(), sample.caps()) else {
                        return Ok(gst::FlowSuccess::Ok);
                    };
                    let st = caps.structure(0).ok_or(gst::FlowError::Error)?;
                    let w = st.get::<i32>("width").map_err(|_| gst::FlowError::Error)? as usize;
                    let h = st.get::<i32>("height").map_err(|_| gst::FlowError::Error)? as usize;
                    let map = buf.map_readable().map_err(|_| gst::FlowError::Error)?;
                    if map.len() >= w * h * 4 {
                        *frame.lock().unwrap() = Some(Frame { w, h, rgba: map[..w * h * 4].to_vec() });
                        ctx2.request_repaint();
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        pipeline.set_state(gst::State::Playing)?;
        let bus = pipeline.bus().context("bus")?;
        let result = loop {
            if !self.enabled.load(Ordering::Relaxed) || *self.device.lock().unwrap() != dev || *self.out_size.lock().unwrap() != size {
                break Ok(());
            }
            if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(200), &[gst::MessageType::Error, gst::MessageType::Eos]) {
                match msg.view() {
                    gst::MessageView::Error(e) => break Err(anyhow!("{}", e.error())),
                    _ => break Err(anyhow!("preview terminou")),
                }
            }
        };
        let _ = pipeline.set_state(gst::State::Null);
        *self.frame.lock().unwrap() = None;
        result
    }
}
