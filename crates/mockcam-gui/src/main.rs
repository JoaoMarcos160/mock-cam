//! mockcam-panel — painel de controle do MockCam (egui).

mod client;
mod preview;

use client::Client;
use eframe::egui;
use mockcam_core::ipc::{Request, Status};
use mockcam_core::source::TEST_PATTERNS;
use mockcam_core::{Effects, GlFilter, GpuMode, OutputCfg, PixelFormat, Rotation, SourceSpec, TextPos};
use preview::Preview;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;

fn main() -> eframe::Result<()> {
    gst::init().expect("GStreamer não inicializou");
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("MockCam")
            .with_app_id("mockcam")
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([820.0, 520.0]),
        vsync: true,
        ..Default::default()
    };
    eframe::run_native("MockCam", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

#[derive(PartialEq, Clone, Copy)]
enum Kind {
    Camera,
    File,
    Image,
    Test,
    Black,
}

#[derive(Clone, Copy)]
enum PickTarget {
    Video,
    Image,
}

struct App {
    client: Client,
    preview: Preview,
    tex: Option<egui::TextureHandle>,

    kind: Kind,
    cam: String,
    cam_fixed_fps: bool,
    file: String,
    file_loop: bool,
    image: String,
    pattern: String,
    rec_secs: u32,

    fx: Effects,
    out: OutputCfg,
    synced: bool,
    seek: Option<f64>,
    speed: f64,

    pick_tx: Sender<(PickTarget, PathBuf)>,
    pick_rx: Receiver<(PickTarget, PathBuf)>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let (pick_tx, pick_rx) = channel();
        let mut style = (*cc.egui_ctx.style()).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        cc.egui_ctx.set_style(style);
        App {
            client: Client::new(cc.egui_ctx.clone()),
            preview: Preview::new(cc.egui_ctx.clone()),
            tex: None,
            kind: Kind::Camera,
            cam: String::new(),
            cam_fixed_fps: true,
            file: String::new(),
            file_loop: true,
            image: String::new(),
            pattern: "smpte".into(),
            rec_secs: 10,
            fx: Effects::default(),
            out: OutputCfg::default(),
            synced: false,
            seek: None,
            speed: 1.0,
            pick_tx,
            pick_rx,
        }
    }

    /// Na primeira conexão, copia o estado do daemon para os controles.
    fn sync_from(&mut self, st: &Status) {
        if self.synced {
            return;
        }
        self.synced = true;
        self.fx = st.effects.clone();
        self.out = st.output.clone();
        self.speed = st.speed;
        match &st.source {
            Some(SourceSpec::Camera { device, fixed_fps }) => {
                self.kind = Kind::Camera;
                self.cam = device.clone();
                self.cam_fixed_fps = *fixed_fps;
            }
            Some(SourceSpec::File { path, looped }) => {
                self.kind = Kind::File;
                self.file = path.clone();
                self.file_loop = *looped;
            }
            Some(SourceSpec::Image { path }) => {
                self.kind = Kind::Image;
                self.image = path.clone();
            }
            Some(SourceSpec::Test { pattern }) => {
                self.kind = Kind::Test;
                self.pattern = pattern.clone();
            }
            Some(SourceSpec::Black) => self.kind = Kind::Black,
            None => {}
        }
        if self.cam.is_empty() {
            if let Some(c) = st.cameras.first() {
                self.cam = c.device.clone();
            }
        }
    }

    fn pick(&self, target: PickTarget) {
        let tx = self.pick_tx.clone();
        // O diálogo bloqueia; roda em outra thread para a UI seguir fluida.
        std::thread::spawn(move || {
            let d = rfd::FileDialog::new();
            let d = match target {
                PickTarget::Video => d.add_filter("Vídeo", &["mp4", "mkv", "mov", "webm", "avi"]),
                PickTarget::Image => d.add_filter("Imagem", &["png", "jpg", "jpeg", "bmp", "webp"]),
            };
            if let Some(p) = d.pick_file() {
                let _ = tx.send((target, p));
            }
        });
    }

    fn source_spec(&self) -> Option<SourceSpec> {
        Some(match self.kind {
            Kind::Camera if !self.cam.is_empty() => SourceSpec::Camera { device: self.cam.clone(), fixed_fps: self.cam_fixed_fps },
            Kind::File if !self.file.is_empty() => SourceSpec::File { path: self.file.clone(), looped: self.file_loop },
            Kind::Image if !self.image.is_empty() => SourceSpec::Image { path: self.image.clone() },
            Kind::Test => SourceSpec::Test { pattern: self.pattern.clone() },
            Kind::Black => SourceSpec::Black,
            _ => return None,
        })
    }

    fn start_daemon() {
        use std::os::unix::process::CommandExt;
        let sibling = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("mockcamd")));
        let bin = sibling.filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("mockcamd"));
        // Preferir o serviço systemd, se instalado; senão roda destacado do painel.
        let via_systemd = std::process::Command::new("systemctl")
            .args(["--user", "start", "mockcam.service"])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if !via_systemd {
            let _ = std::process::Command::new(bin)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .process_group(0)
                .spawn();
        }
    }

    // ---------------- UI ----------------

    fn ui_source(&mut self, ui: &mut egui::Ui, st: &Status) {
        ui.heading("Fonte");
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.kind, Kind::Camera, "📷 Câmera");
            ui.selectable_value(&mut self.kind, Kind::File, "🎞 Vídeo");
            ui.selectable_value(&mut self.kind, Kind::Image, "🖼 Imagem");
            ui.selectable_value(&mut self.kind, Kind::Test, "▦ Teste");
            ui.selectable_value(&mut self.kind, Kind::Black, "■ Preto");
        });
        match self.kind {
            Kind::Camera => {
                egui::ComboBox::from_label("Dispositivo")
                    .selected_text(
                        st.cameras.iter().find(|c| c.device == self.cam).map(|c| format!("{} — {}", c.device, c.name)).unwrap_or(self.cam.clone()),
                    )
                    .show_ui(ui, |ui| {
                        for c in &st.cameras {
                            ui.selectable_value(&mut self.cam, c.device.clone(), format!("{} — {}", c.device, c.name));
                        }
                    });
                ui.checkbox(&mut self.cam_fixed_fps, "FPS fixo (não cair para ~10 fps no escuro)")
                    .on_hover_text("Desliga a exposição dinâmica da webcam. A imagem fica um pouco mais escura em pouca luz.");
                if st.cameras.is_empty() {
                    ui.colored_label(egui::Color32::YELLOW, "Nenhuma câmera encontrada");
                }
            }
            Kind::File => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.file).hint_text("/caminho/video.mp4").desired_width(220.0));
                    if ui.button("Escolher…").clicked() {
                        self.pick(PickTarget::Video);
                    }
                });
                ui.checkbox(&mut self.file_loop, "Loop contínuo");
            }
            Kind::Image => {
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut self.image).hint_text("/caminho/imagem.png").desired_width(220.0));
                    if ui.button("Escolher…").clicked() {
                        self.pick(PickTarget::Image);
                    }
                });
            }
            Kind::Test => {
                egui::ComboBox::from_label("Padrão").selected_text(&self.pattern).show_ui(ui, |ui| {
                    for p in TEST_PATTERNS {
                        ui.selectable_value(&mut self.pattern, p.to_string(), *p);
                    }
                });
            }
            Kind::Black => {}
        }
        let spec = self.source_spec();
        let differs = spec.is_some() && spec != st.source;
        if ui.add_enabled(spec.is_some(), egui::Button::new(if differs { "▶ Colocar no ar" } else { "↻ Reiniciar fonte" })).clicked() {
            if let Some(source) = spec {
                self.client.send(Request::SetSource { source });
            }
        }

        ui.separator();
        ui.heading("Loop da fonte atual");
        ui.label("Grava os últimos segundos do que está no ar (ex.: sua câmera) e repete em loop.");
        ui.horizontal(|ui| {
            ui.add(egui::Slider::new(&mut self.rec_secs, 2..=60).suffix(" s"));
            let busy = st.recording.is_some();
            if ui.add_enabled(!busy, egui::Button::new("⏺ Gravar e loopar")).clicked() {
                self.client.send(Request::Record { secs: self.rec_secs, then_loop: true });
            }
        });
        if let Some(p) = st.recording {
            ui.add(egui::ProgressBar::new(p as f32).text(format!("gravando… {:.0}%", p * 100.0)));
        }
        if let Some(clip) = &st.last_clip {
            ui.horizontal(|ui| {
                ui.label("Último clipe:");
                if ui.small_button("▶ tocar de novo").clicked() {
                    self.client.send(Request::SetSource { source: SourceSpec::File { path: clip.clone(), looped: true } });
                }
            });
        }
        if let Some(cam) = st.cameras.first() {
            if !matches!(st.source, Some(SourceSpec::Camera { .. })) && ui.button("↩ Voltar para câmera ao vivo").clicked() {
                let device = if self.cam.is_empty() { cam.device.clone() } else { self.cam.clone() };
                self.client.send(Request::SetSource { source: SourceSpec::Camera { device, fixed_fps: self.cam_fixed_fps } });
            }
        }
    }

    fn ui_playback(&mut self, ui: &mut egui::Ui, st: &Status) {
        ui.heading("Reprodução");
        ui.horizontal(|ui| {
            let seekable = st.source.as_ref().is_some_and(|s| s.is_seekable());
            if st.source_state == "paused" {
                if ui.button("▶ Continuar").clicked() {
                    self.client.send(Request::Resume);
                }
            } else if ui.add_enabled(st.source.is_some(), egui::Button::new("⏸ Pausar")).clicked() {
                self.client.send(Request::Pause);
            }
            let mut frozen = st.frozen;
            if ui.toggle_value(&mut frozen, "❄ Congelar saída").changed() {
                self.client.send(Request::Freeze { on: frozen });
            }
            if !seekable {
                return;
            }
            if st.source_state == "ended" && ui.button("⟲ Do início").clicked() {
                self.client.send(Request::Seek { secs: 0.0 });
            }
        });
        if let (Some(pos), Some(dur)) = (st.position, st.duration) {
            if dur > 0.0 {
                let mut v = self.seek.unwrap_or(pos);
                let label = format!("{} / {}", mmss(v), mmss(dur));
                let r = ui.add(egui::Slider::new(&mut v, 0.0..=dur).show_value(false).text(label));
                if r.dragged() {
                    self.seek = Some(v);
                }
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.client.send(Request::Seek { secs: v });
                    self.seek = None;
                }
            }
            let r = ui.add(egui::Slider::new(&mut self.speed, 0.25..=4.0).logarithmic(true).text("velocidade").suffix("x"));
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                self.client.send(Request::Speed { rate: self.speed });
            }
        }
    }

    fn ui_effects(&mut self, ui: &mut egui::Ui, st: &Status) {
        ui.heading("Efeitos");
        let before = self.fx.clone();
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.fx.mirror, "Espelhar");
            egui::ComboBox::from_id_salt("rot")
                .selected_text(match self.fx.rotation {
                    Rotation::R0 => "0°",
                    Rotation::R90 => "90°",
                    Rotation::R180 => "180°",
                    Rotation::R270 => "270°",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.fx.rotation, Rotation::R0, "0°");
                    ui.selectable_value(&mut self.fx.rotation, Rotation::R90, "90°");
                    ui.selectable_value(&mut self.fx.rotation, Rotation::R180, "180°");
                    ui.selectable_value(&mut self.fx.rotation, Rotation::R270, "270°");
                });
            ui.label("rotação");
        });
        ui.add(egui::Slider::new(&mut self.fx.brightness, -1.0..=1.0).text("brilho"));
        ui.add(egui::Slider::new(&mut self.fx.contrast, 0.0..=2.0).text("contraste"));
        ui.add(egui::Slider::new(&mut self.fx.saturation, 0.0..=2.0).text("saturação"));
        ui.add(egui::Slider::new(&mut self.fx.hue, -1.0..=1.0).text("matiz"));
        ui.add(egui::Slider::new(&mut self.fx.zoom, 1.0..=4.0).text("zoom").suffix("x"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.fx.text).hint_text("texto sobreposto").desired_width(170.0));
            egui::ComboBox::from_id_salt("tpos").selected_text(format!("{:?}", self.fx.text_pos)).show_ui(ui, |ui| {
                for p in [TextPos::TopLeft, TextPos::TopCenter, TextPos::TopRight, TextPos::BottomLeft, TextPos::BottomCenter, TextPos::BottomRight] {
                    ui.selectable_value(&mut self.fx.text_pos, p, format!("{p:?}"));
                }
            });
        });
        ui.checkbox(&mut self.fx.text_mirrored, "Usar texto invertido")
            .on_hover_text("Fica legível no seu preview espelhado (Meet/Zoom/Teams), mas invertido para os outros.");
        let gl_ok = st.gpu.gl && st.backend != mockcam_core::gpu::Backend::Cpu;
        ui.add_enabled_ui(gl_ok, |ui| {
            egui::ComboBox::from_label("Filtro GL (GPU)").selected_text(format!("{:?}", self.fx.gl_filter)).show_ui(ui, |ui| {
                for f in GlFilter::ALL {
                    ui.selectable_value(&mut self.fx.gl_filter, f, format!("{f:?}"));
                }
            });
        });
        if !gl_ok {
            ui.small("Filtros GL exigem GPU ligada.");
        }
        if ui.button("Restaurar padrão").clicked() {
            self.fx = Effects::default();
        }
        if self.fx != before {
            self.client.send(Request::SetEffects { effects: self.fx.clone() });
        }
    }

    fn ui_gpu(&mut self, ui: &mut egui::Ui, st: &Status) {
        ui.heading("Aceleração (GPU)");
        let mut mode = st.gpu_mode;
        ui.horizontal(|ui| {
            ui.selectable_value(&mut mode, GpuMode::Off, "Desligada");
            ui.selectable_value(&mut mode, GpuMode::Auto, "Auto");
            ui.add_enabled_ui(st.gpu.vaapi, |ui| ui.selectable_value(&mut mode, GpuMode::Intel, "Intel (VA-API)"));
            ui.add_enabled_ui(st.gpu.nvidia, |ui| ui.selectable_value(&mut mode, GpuMode::Nvidia, "NVIDIA"));
        });
        if mode != st.gpu_mode {
            self.client.send(Request::SetGpu { mode });
        }
        ui.label(format!(
            "Ativo: {:?}{}   ·   disponível: VA-API {}  NVIDIA {}  GL {}",
            st.backend,
            if st.gl_active { " + filtro GL" } else { "" },
            yn(st.gpu.vaapi),
            yn(st.gpu.nvidia),
            yn(st.gpu.gl)
        ));
        if let Some(n) = &st.gpu_note {
            ui.colored_label(egui::Color32::YELLOW, n);
        }
    }

    fn ui_output(&mut self, ui: &mut egui::Ui, st: &Status) {
        egui::CollapsingHeader::new("Saída (dispositivo virtual)").default_open(false).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label("Dispositivo");
                ui.text_edit_singleline(&mut self.out.device);
            });
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut self.out.width).range(160..=3840).speed(8).prefix("L "));
                ui.add(egui::DragValue::new(&mut self.out.height).range(120..=2160).speed(2).prefix("A "));
                ui.add(egui::DragValue::new(&mut self.out.fps).range(1..=120).suffix(" fps"));
                egui::ComboBox::from_id_salt("fmt").selected_text(self.out.format.gst()).show_ui(ui, |ui| {
                    for f in PixelFormat::ALL {
                        ui.selectable_value(&mut self.out.format, f, f.gst());
                    }
                });
            });
            ui.horizontal(|ui| {
                for (l, w, h) in [("480p", 848, 480), ("720p", 1280, 720), ("1080p", 1920, 1080)] {
                    if ui.small_button(l).clicked() {
                        self.out.width = w;
                        self.out.height = h;
                    }
                }
            });
            ui.small("Feche apps usando a câmera antes de mudar resolução/formato.");
            if ui.add_enabled(self.out != st.output, egui::Button::new("Aplicar saída")).clicked() {
                self.client.send(Request::SetOutput { output: self.out.clone() });
            }
        });
    }

    fn ui_preview(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, st: Option<&Status>) {
        ui.horizontal(|ui| {
            let mut on = self.preview.enabled.load(Ordering::Relaxed);
            if ui.checkbox(&mut on, "Preview").changed() {
                self.preview.enabled.store(on, Ordering::Relaxed);
            }
            if let Some(st) = st {
                let src = st.source.as_ref().map(|s| s.label()).unwrap_or_else(|| "—".into());
                ui.label(format!(
                    "{src} · {}   |   saída {:.1} fps · fonte {:.1} fps",
                    st.source_state, st.out_fps, st.in_fps
                ));
            }
        });

        if let Some(f) = self.preview.frame.lock().unwrap().take() {
            let img = egui::ColorImage::from_rgba_unmultiplied([f.w, f.h], &f.rgba);
            match &mut self.tex {
                Some(t) => t.set(img, egui::TextureOptions::LINEAR),
                None => self.tex = Some(ctx.load_texture("preview", img, egui::TextureOptions::LINEAR)),
            }
        }
        let avail = ui.available_size();
        match (&self.tex, self.preview.enabled.load(Ordering::Relaxed)) {
            (Some(t), true) => {
                let [w, h] = t.size();
                let scale = (avail.x / w as f32).min(avail.y / h as f32);
                let size = egui::vec2(w as f32 * scale, h as f32 * scale);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(t.id(), size)));
                });
            }
            _ => {
                ui.centered_and_justified(|ui| {
                    let msg = self.preview.error.lock().unwrap().clone().unwrap_or_else(|| "preview desligado / aguardando frames".into());
                    ui.label(msg);
                });
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok((t, p)) = self.pick_rx.try_recv() {
            let s = p.to_string_lossy().to_string();
            match t {
                PickTarget::Video => {
                    self.file = s;
                    self.kind = Kind::File;
                }
                PickTarget::Image => {
                    self.image = s;
                    self.kind = Kind::Image;
                }
            }
        }

        let st = self.client.status.lock().unwrap().clone();
        if let Some(st) = &st {
            self.sync_from(st);
            *self.preview.device.lock().unwrap() = st.output.device.clone();
            *self.preview.out_size.lock().unwrap() = (st.output.width, st.output.height);
        } else {
            self.synced = false;
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("MockCam");
                if self.client.connected.load(Ordering::Relaxed) {
                    ui.colored_label(egui::Color32::LIGHT_GREEN, "● daemon conectado");
                } else {
                    ui.colored_label(egui::Color32::LIGHT_RED, "● daemon parado");
                    if ui.button("Iniciar daemon").clicked() {
                        Self::start_daemon();
                    }
                }
                if let Some(st) = &st {
                    if !st.device_ok {
                        ui.colored_label(egui::Color32::YELLOW, "⚠ dispositivo virtual ausente");
                    }
                    if ui.small_button("Encerrar daemon").clicked() {
                        self.client.send(Request::Shutdown);
                    }
                }
            });
            let err = self.client.error.lock().unwrap().clone();
            for e in [err, st.as_ref().and_then(|s| s.last_error.clone()), st.as_ref().and_then(|s| s.output_error.clone())]
                .into_iter()
                .flatten()
            {
                ui.colored_label(egui::Color32::from_rgb(255, 140, 120), e);
            }
        });

        egui::SidePanel::left("controls").resizable(true).default_width(400.0).show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match &st {
                Some(st) => {
                    self.ui_source(ui, st);
                    ui.separator();
                    self.ui_playback(ui, st);
                    ui.separator();
                    self.ui_effects(ui, st);
                    ui.separator();
                    self.ui_gpu(ui, st);
                    ui.separator();
                    self.ui_output(ui, st);
                }
                None => {
                    ui.label("Aguardando o daemon (mockcamd)…");
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| self.ui_preview(ui, ctx, st.as_ref()));

        // Sem preview: redesenha só para atualizar status (barato).
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

fn mmss(s: f64) -> String {
    let s = s.max(0.0) as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn yn(b: bool) -> &'static str {
    if b {
        "✔"
    } else {
        "✖"
    }
}
