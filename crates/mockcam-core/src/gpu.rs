//! Detecção de aceleração por hardware e seleção via *rank* de features.
//!
//! O `decodebin` escolhe decoders pelo rank no registry. Para ligar/desligar a
//! GPU basta mudar o rank dos decoders HW e reconstruir só o pipeline da fonte.

use gst::prelude::*;
use serde::{Deserialize, Serialize};

use crate::GpuMode;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Cpu,
    /// Intel/AMD via VA-API (plugins `vaapi` ou `va`)
    Vaapi,
    /// NVIDIA via NVDEC/NVENC (plugin `nvcodec`)
    Nvidia,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default)]
pub struct GpuInfo {
    pub vaapi: bool,
    pub nvidia: bool,
    pub gl: bool,
}

fn hw_family(plugin: &str) -> Option<Backend> {
    match plugin {
        "vaapi" | "va" => Some(Backend::Vaapi),
        "nvcodec" => Some(Backend::Nvidia),
        _ => None,
    }
}

/// Itera os decoders de vídeo/imagem que pertencem a um plugin de HW.
fn for_each_hw_decoder(mut f: impl FnMut(&gst::PluginFeature, Backend)) {
    let reg = gst::Registry::get();
    for feat in reg.features(gst::ElementFactory::static_type()) {
        let Some(factory) = feat.downcast_ref::<gst::ElementFactory>() else { continue };
        let klass = factory.metadata(gst::ELEMENT_METADATA_KLASS).unwrap_or("");
        if !klass.contains("Decoder") || !(klass.contains("Video") || klass.contains("Image")) {
            continue;
        }
        let Some(plugin) = feat.plugin_name() else { continue };
        if let Some(fam) = hw_family(plugin.as_str()) {
            f(&feat, fam);
        }
    }
}

pub fn detect() -> GpuInfo {
    let mut info = GpuInfo::default();
    for_each_hw_decoder(|_, fam| match fam {
        Backend::Vaapi => info.vaapi = true,
        Backend::Nvidia => info.nvidia = true,
        Backend::Cpu => {}
    });
    info.gl = gst::ElementFactory::find("glupload").is_some()
        && gst::ElementFactory::find("gleffects").is_some()
        && gst::ElementFactory::find("glcolorbalance").is_some();
    info
}

/// Modo pedido pelo usuário → backend efetivo, conforme o que existe.
/// Em notebooks híbridos o padrão é a iGPU (VA-API): não acorda a dGPU.
pub fn resolve(mode: GpuMode, info: &GpuInfo) -> Backend {
    match mode {
        GpuMode::Off => Backend::Cpu,
        GpuMode::Intel if info.vaapi => Backend::Vaapi,
        GpuMode::Nvidia if info.nvidia => Backend::Nvidia,
        GpuMode::Auto if info.vaapi => Backend::Vaapi,
        GpuMode::Auto if info.nvidia => Backend::Nvidia,
        _ => Backend::Cpu,
    }
}

/// Ajusta o rank: decoders do backend escolhido ficam acima dos de software;
/// os demais decoders de HW ficam com rank NONE (nunca auto-selecionados).
pub fn apply(backend: Backend) {
    for_each_hw_decoder(|feat, fam| {
        // Elementos com defeito no gstreamer-vaapi 1.20 + driver iHD:
        // - vaapidecodebin: embute o vaapipostproc, que entrega frames vazios (verde);
        // - vaapijpegdec: lê o MJPEG (faixa completa) como faixa limitada → saturação ~2x.
        // - nvjpegdec: falha com o MJPEG de webcams UVC (a câmera caía para YUYV a ~10 fps).
        // O MJPEG da webcam fica no jpegdec (CPU, barato em 720p); vídeos seguem na GPU.
        let broken = matches!(feat.name().as_str(), "vaapidecodebin" | "vaapijpegdec" | "nvjpegdec");
        let rank = if fam == backend && !broken { gst::Rank::PRIMARY + 10 } else { gst::Rank::NONE };
        feat.set_rank(rank);
    });
    tracing::info!("decoders de hardware: {backend:?}");
}

/// Encoder H.264 para gravar o loop, preferindo o backend ativo.
pub fn h264_encoder(backend: Backend) -> String {
    let has = |n: &str| gst::ElementFactory::find(n).is_some();
    match backend {
        Backend::Vaapi if has("vaapih264enc") => "vaapih264enc bitrate=8000".into(),
        Backend::Vaapi if has("vah264enc") => "vah264enc bitrate=8000".into(),
        Backend::Nvidia if has("nvh264enc") => "nvh264enc bitrate=8000".into(),
        _ => "x264enc tune=zerolatency speed-preset=ultrafast bitrate=8000 key-int-max=30".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_prefers_igpu() {
        let both = GpuInfo { vaapi: true, nvidia: true, gl: true };
        assert_eq!(resolve(GpuMode::Auto, &both), Backend::Vaapi);
        assert_eq!(resolve(GpuMode::Nvidia, &both), Backend::Nvidia);
        assert_eq!(resolve(GpuMode::Off, &both), Backend::Cpu);
        let none = GpuInfo::default();
        assert_eq!(resolve(GpuMode::Intel, &none), Backend::Cpu);
    }
}
