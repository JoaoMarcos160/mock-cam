use anyhow::{anyhow, bail, Context, Result};
use gst::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::{OutputCfg, PixelFormat};

pub fn launch(desc: &str) -> Result<gst::Pipeline> {
    let el = gst::parse::launch(desc).with_context(|| format!("pipeline inválido: {desc}"))?;
    el.downcast::<gst::Pipeline>().map_err(|_| anyhow!("não é um pipeline: {desc}"))
}

pub fn by_name<T: IsA<gst::Element>>(p: &gst::Pipeline, name: &str) -> Result<T> {
    p.by_name(name)
        .ok_or_else(|| anyhow!("elemento '{name}' ausente"))?
        .downcast::<T>()
        .map_err(|_| anyhow!("elemento '{name}' com tipo inesperado"))
}

pub fn error_text(e: &gst::message::Error) -> String {
    let src = e.src().map(|s| s.name().to_string()).unwrap_or_default();
    match e.debug() {
        Some(d) => format!("{src}: {} ({d})", e.error()),
        None => format!("{src}: {}", e.error()),
    }
}

/// Liga o appsink a um callback que recebe cada buffer (sem cópia).
pub fn on_buffer(sink: &gst_app::AppSink, f: impl Fn(gst::Buffer) + Send + Sync + 'static) {
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(buf) = sample.buffer_owned() {
                    f(buf);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
}

/// Espera o primeiro frame ou um erro no bus. Usado para validar uma fonte
/// antes de trocar a fonte atual por ela (make-before-break).
pub fn wait_first_frame(p: &gst::Pipeline, got: &AtomicBool, timeout: Duration) -> Result<()> {
    let bus = p.bus().context("pipeline sem bus")?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if got.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(msg) = bus.timed_pop_filtered(gst::ClockTime::from_mseconds(20), &[gst::MessageType::Error]) {
            if let gst::MessageView::Error(e) = msg.view() {
                bail!(error_text(e));
            }
        }
    }
    if got.load(Ordering::Relaxed) {
        return Ok(());
    }
    bail!("nenhum frame recebido em {:?}", timeout)
}

/// Thread que observa o bus até `stop` ser sinalizado.
pub fn spawn_bus_watch(
    p: &gst::Pipeline,
    stop: Arc<AtomicBool>,
    mut handler: impl FnMut(&gst::Message) + Send + 'static,
) -> Option<JoinHandle<()>> {
    let bus = p.bus()?;
    Some(std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                handler(&msg);
            }
        }
    }))
}

/// Para o pipeline fora da thread chamadora (set_state(Null) pode demorar).
pub fn stop_async(p: gst::Pipeline, stop: Arc<AtomicBool>, watch: Option<JoinHandle<()>>) {
    std::thread::spawn(move || stop_blocking(p, stop, watch));
}

pub fn stop_blocking(p: gst::Pipeline, stop: Arc<AtomicBool>, watch: Option<JoinHandle<()>>) {
    stop.store(true, Ordering::Relaxed);
    let _ = p.set_state(gst::State::Null);
    if let Some(h) = watch {
        let _ = h.join();
    }
}

/// Frame preto no formato de saída (usado antes do primeiro frame da fonte).
pub fn black_frame(o: &OutputCfg) -> gst::Buffer {
    let (w, h) = (o.width as usize, o.height as usize);
    let data = match o.format {
        PixelFormat::Yuy2 => [16u8, 128].repeat(w * h),
        PixelFormat::I420 | PixelFormat::Nv12 => {
            let mut v = vec![16u8; w * h];
            v.resize(w * h + w * h / 2, 128);
            v
        }
    };
    gst::Buffer::from_mut_slice(data)
}
