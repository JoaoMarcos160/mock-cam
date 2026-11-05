//! Pipeline de efeitos: appsrc → [crop/flip/escala] → [balanço/filtro: CPU ou GL] → texto → appsink.
//!
//! Ajustes contínuos (brilho, espelho, zoom, texto...) são aplicados AO VIVO por
//! propriedades. O pipeline só é reconstruído quando muda CPU↔GL, e a saída
//! segue repetindo o último frame durante a troca.

use anyhow::Result;
use gst::prelude::*;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::engine::Event;
use crate::gpu::{Backend, GpuInfo};
use crate::slot::FrameRouter;
use crate::util;
use crate::{Effects, GlFilter, OutputCfg, Rotation, TextPos};

pub struct EffectsChain {
    pipeline: gst::Pipeline,
    pub src: gst_app::AppSrc,
    pub gl: bool,
    stop: Arc<AtomicBool>,
    watch: Option<JoinHandle<()>>,
}

/// GL só entra quando há filtro GL escolhido, GPU ligada e plugin GL presente.
/// Sem filtro GL, os ajustes de CPU custam ~nada em 720p e evitam upload/download.
pub fn wants_gl(fx: &Effects, backend: Backend, info: &GpuInfo) -> bool {
    fx.gl_filter != GlFilter::None && backend != Backend::Cpu && info.gl
}

fn direction(fx: &Effects) -> &'static str {
    match (fx.mirror, fx.rotation) {
        (false, Rotation::R0) => "identity",
        (false, Rotation::R90) => "90r",
        (false, Rotation::R180) => "180",
        (false, Rotation::R270) => "90l",
        (true, Rotation::R0) => "horiz",
        (true, Rotation::R90) => "ul-lr",
        (true, Rotation::R180) => "vert",
        (true, Rotation::R270) => "ur-ll",
    }
}

fn even(v: f64) -> i32 {
    ((v.max(0.0) as i32) / 2) * 2
}

impl EffectsChain {
    pub fn build(out: &OutputCfg, fx: &Effects, use_gl: bool, router: Arc<FrameRouter>, events: Sender<Event>) -> Result<Self> {
        let caps = out.caps_str();
        let color = if use_gl {
            // Bloco GL contínuo: um upload, um download.
            format!("glupload ! glcolorconvert ! glcolorbalance name=bal ! gleffects name=gl ! glcolorconvert ! gldownload ! videoconvert ! {caps} !")
        } else {
            "videobalance name=bal !".to_string()
        };
        let desc = format!(
            "appsrc name=src format=time ! videocrop name=crop ! videoflip name=flip ! \
             videoscale add-borders=true ! {caps} ! {color} \
             videoflip name=tflip_a ! textoverlay name=txt shaded-background=true font-desc=\"Sans 28\" ! \
             videoflip name=tflip_b ! \
             appsink name=sink sync=false max-buffers=1 drop=true enable-last-sample=false"
        );
        let p = util::launch(&desc)?;

        let src: gst_app::AppSrc = util::by_name(&p, "src")?;
        src.set_caps(Some(&gst::Caps::from_str(&caps)?));
        src.set_format(gst::Format::Time);
        if src.find_property("leaky-type").is_some() {
            // GStreamer ≥ 1.20: nunca bloqueia a fonte; descarta o frame mais velho.
            src.set_property("max-buffers", 2u64);
            src.set_property_from_str("leaky-type", "downstream");
        }

        let sink: gst_app::AppSink = util::by_name(&p, "sink")?;
        {
            let router = router.clone();
            util::on_buffer(&sink, move |buf| router.out.put(buf));
        }

        let chain = EffectsChain { pipeline: p, src, gl: use_gl, stop: Arc::new(AtomicBool::new(false)), watch: None };
        chain.apply(out, fx);
        chain.pipeline.set_state(gst::State::Playing)?;

        let watch = util::spawn_bus_watch(&chain.pipeline, chain.stop.clone(), move |msg| {
            if let gst::MessageView::Error(e) = msg.view() {
                let _ = events.send(Event::EffectsError { gl: use_gl, msg: util::error_text(e) });
            }
        });
        Ok(EffectsChain { watch, ..chain })
    }

    /// Aplica parâmetros ao vivo (sem reconstruir).
    pub fn apply(&self, out: &OutputCfg, fx: &Effects) {
        let p = &self.pipeline;
        if let Some(crop) = p.by_name("crop") {
            let z = fx.zoom.clamp(1.0, 4.0);
            let cw = out.width as f64 * (1.0 - 1.0 / z) / 2.0;
            let ch = out.height as f64 * (1.0 - 1.0 / z) / 2.0;
            crop.set_property("left", even(cw));
            crop.set_property("right", even(cw));
            crop.set_property("top", even(ch));
            crop.set_property("bottom", even(ch));
        }
        if let Some(flip) = p.by_name("flip") {
            flip.set_property_from_str("video-direction", direction(fx));
        }
        if let Some(bal) = p.by_name("bal") {
            bal.set_property("brightness", fx.brightness.clamp(-1.0, 1.0));
            bal.set_property("contrast", fx.contrast.clamp(0.0, 2.0));
            bal.set_property("saturation", fx.saturation.clamp(0.0, 2.0));
            bal.set_property("hue", fx.hue.clamp(-1.0, 1.0));
        }
        if let Some(gl) = p.by_name("gl") {
            gl.set_property_from_str("effect", fx.gl_filter.nick());
        }
        // Texto invertido: espelha → desenha o texto → espelha de volta (só o texto fica invertido).
        let tdir = if fx.text_mirrored && !fx.text.trim().is_empty() { "horiz" } else { "identity" };
        for n in ["tflip_a", "tflip_b"] {
            if let Some(f) = p.by_name(n) {
                f.set_property_from_str("video-direction", tdir);
            }
        }
        if let Some(txt) = p.by_name("txt") {
            txt.set_property("text", &fx.text);
            txt.set_property("silent", fx.text.trim().is_empty());
            let (v, h) = match fx.text_pos {
                TextPos::TopLeft => ("top", "left"),
                TextPos::TopCenter => ("top", "center"),
                TextPos::TopRight => ("top", "right"),
                TextPos::BottomLeft => ("bottom", "left"),
                TextPos::BottomCenter => ("bottom", "center"),
                TextPos::BottomRight => ("bottom", "right"),
            };
            txt.set_property_from_str("valignment", v);
            txt.set_property_from_str("halignment", h);
        }
    }

    pub fn stop(self) {
        util::stop_async(self.pipeline, self.stop, self.watch);
    }
}

