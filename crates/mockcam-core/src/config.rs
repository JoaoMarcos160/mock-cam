use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    #[default]
    Yuy2,
    I420,
    Nv12,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 3] = [PixelFormat::Yuy2, PixelFormat::I420, PixelFormat::Nv12];
    pub fn gst(self) -> &'static str {
        match self {
            PixelFormat::Yuy2 => "YUY2",
            PixelFormat::I420 => "I420",
            PixelFormat::Nv12 => "NV12",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct OutputCfg {
    pub device: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub format: PixelFormat,
}

impl Default for OutputCfg {
    fn default() -> Self {
        Self { device: "/dev/video10".into(), width: 1280, height: 720, fps: 30, format: PixelFormat::Yuy2 }
    }
}

impl OutputCfg {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.width >= 160 && self.width <= 3840 && self.width % 8 == 0, "largura deve ser múltiplo de 8 (160..3840)");
        anyhow::ensure!(self.height >= 120 && self.height <= 2160 && self.height % 2 == 0, "altura deve ser par (120..2160)");
        anyhow::ensure!((1..=120).contains(&self.fps), "fps deve estar entre 1 e 120");
        Ok(())
    }

    /// Caps raw sem framerate (o framerate é imposto pela thread de saída).
    pub fn caps_str(&self) -> String {
        format!(
            "video/x-raw,format={},width={},height={},pixel-aspect-ratio=1/1",
            self.format.gst(),
            self.width,
            self.height
        )
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceSpec {
    Camera {
        device: String,
        /// Desliga `exposure_dynamic_framerate` da webcam: fps constante mesmo no escuro
        /// (imagem um pouco mais escura). Sem isso, muitas UVC caem para ~10 fps em pouca luz.
        #[serde(default = "yes")]
        fixed_fps: bool,
    },
    File {
        path: String,
        #[serde(default = "yes")]
        looped: bool,
    },
    Image { path: String },
    Test {
        #[serde(default = "smpte")]
        pattern: String,
    },
    Black,
}

fn yes() -> bool {
    true
}
fn smpte() -> String {
    "smpte".into()
}

impl SourceSpec {
    pub fn label(&self) -> String {
        match self {
            SourceSpec::Camera { device, .. } => format!("Câmera {device}"),
            SourceSpec::File { path, looped } => {
                format!("Vídeo {}{}", short(path), if *looped { " (loop)" } else { "" })
            }
            SourceSpec::Image { path } => format!("Imagem {}", short(path)),
            SourceSpec::Test { pattern } => format!("Teste ({pattern})"),
            SourceSpec::Black => "Preto".into(),
        }
    }
    pub fn is_seekable(&self) -> bool {
        matches!(self, SourceSpec::File { .. })
    }
}

fn short(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GpuMode {
    Off,
    #[default]
    Auto,
    Intel,
    Nvidia,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TextPos {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    #[default]
    BottomCenter,
    BottomRight,
}

/// Filtros GL (rodam na GPU via `gleffects`). Só ativos com GPU ligada.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum GlFilter {
    #[default]
    None,
    Sepia,
    Heat,
    Xray,
    Xpro,
    Blur,
    Glow,
    Sobel,
    Mirror,
    Squeeze,
    Fisheye,
    Twirl,
    Bulge,
    Tunnel,
}

impl GlFilter {
    pub const ALL: [GlFilter; 14] = [
        GlFilter::None,
        GlFilter::Sepia,
        GlFilter::Heat,
        GlFilter::Xray,
        GlFilter::Xpro,
        GlFilter::Blur,
        GlFilter::Glow,
        GlFilter::Sobel,
        GlFilter::Mirror,
        GlFilter::Squeeze,
        GlFilter::Fisheye,
        GlFilter::Twirl,
        GlFilter::Bulge,
        GlFilter::Tunnel,
    ];
    /// Nick do enum `effect` do elemento `gleffects`.
    pub fn nick(self) -> &'static str {
        match self {
            GlFilter::None => "identity",
            GlFilter::Sepia => "sepia",
            GlFilter::Heat => "heat",
            GlFilter::Xray => "xray",
            GlFilter::Xpro => "xpro",
            GlFilter::Blur => "blur",
            GlFilter::Glow => "glow",
            GlFilter::Sobel => "sobel",
            GlFilter::Mirror => "mirror",
            GlFilter::Squeeze => "squeeze",
            GlFilter::Fisheye => "fisheye",
            GlFilter::Twirl => "twirl",
            GlFilter::Bulge => "bulge",
            GlFilter::Tunnel => "tunnel",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Effects {
    pub mirror: bool,
    pub rotation: Rotation,
    /// -1..1
    pub brightness: f64,
    /// 0..2
    pub contrast: f64,
    /// 0..2
    pub saturation: f64,
    /// -1..1
    pub hue: f64,
    /// 1..4 (zoom digital central)
    pub zoom: f64,
    pub text: String,
    pub text_pos: TextPos,
    /// Desenha o texto espelhado: legível no preview espelhado do próprio app
    /// (Meet/Zoom/Teams), mas invertido para os outros participantes.
    pub text_mirrored: bool,
    pub gl_filter: GlFilter,
}

impl Default for Effects {
    fn default() -> Self {
        Self {
            mirror: false,
            rotation: Rotation::R0,
            brightness: 0.0,
            contrast: 1.0,
            saturation: 1.0,
            hue: 0.0,
            zoom: 1.0,
            text: String::new(),
            text_pos: TextPos::BottomCenter,
            text_mirrored: false,
            gl_filter: GlFilter::None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub output: OutputCfg,
    pub source: SourceSpec,
    pub gpu: GpuMode,
    pub effects: Effects,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output: OutputCfg::default(),
            source: SourceSpec::Test { pattern: "smpte".into() },
            gpu: GpuMode::Auto,
            effects: Effects::default(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".config"));
        base.join("mockcam").join("config.toml")
    }

    pub fn load() -> Config {
        let p = Self::path();
        match std::fs::read_to_string(&p) {
            Ok(s) => toml::from_str(&s).unwrap_or_else(|e| {
                tracing::warn!("config inválida em {}: {e}; usando padrão", p.display());
                Config::default()
            }),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) {
        let p = Self::path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match toml::to_string_pretty(self) {
            Ok(s) => {
                // Escrita atômica: tmp + rename.
                let tmp = p.with_extension("toml.tmp");
                if std::fs::write(&tmp, s).is_ok() {
                    let _ = std::fs::rename(&tmp, &p);
                }
            }
            Err(e) => tracing::warn!("falha ao serializar config: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip() {
        let mut c = Config::default();
        c.source = SourceSpec::File { path: "/tmp/a b.mp4".into(), looped: true };
        c.effects.gl_filter = GlFilter::Sepia;
        let s = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.source, c.source);
        assert_eq!(back.effects, c.effects);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let c: Config = toml::from_str("[output]\nfps = 60\n").unwrap();
        assert_eq!(c.output.fps, 60);
        assert_eq!(c.output.width, 1280);
    }

    #[test]
    fn output_validation() {
        let mut o = OutputCfg::default();
        assert!(o.validate().is_ok());
        o.width = 1281;
        assert!(o.validate().is_err());
    }
}
