//! Orquestra fonte, efeitos, saída, GPU e gravação.

use anyhow::{anyhow, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::effects::{self, EffectsChain};
use crate::gpu::{self, Backend, GpuInfo};
use crate::ipc::{Request, Response, Status};
use crate::output::{OutStats, Output};
use crate::slot::FrameRouter;
use crate::source::{self, Source};
use crate::{recorder, Config, Effects, GpuMode, OutputCfg, SourceSpec};

pub enum Event {
    SourceError { gen: u64, msg: String },
    SourceEos { gen: u64 },
    EffectsError { gl: bool, msg: String },
    RecordDone { path: String, then_loop: bool },
    /// `gpu_enc`: falhou com encoder de GPU (refaz pela CPU).
    RecordFailed { msg: String, secs: u32, then_loop: bool, gpu_enc: bool },
}

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

struct Inner {
    cfg: Mutex<Config>,
    gpu_info: GpuInfo,
    backend: Mutex<Backend>,
    gpu_note: Mutex<Option<String>>,
    router: Arc<FrameRouter>,
    stats: Arc<OutStats>,
    output: Mutex<Option<Output>>,
    effects: Mutex<Option<EffectsChain>>,
    source: Mutex<Option<Source>>,
    /// Serializa trocas de fonte/saída (comandos concorrentes do painel e da CLI).
    switch: Mutex<()>,
    gen: AtomicU64,
    source_state: Mutex<String>,
    last_error: Mutex<Option<String>>,
    recording: Arc<Mutex<Option<f64>>>,
    last_clip: Mutex<Option<String>>,
    events: Sender<Event>,
}

impl Engine {
    pub fn new(cfg: Config) -> Result<Engine> {
        let (tx, rx) = channel();
        let gpu_info = gpu::detect();
        tracing::info!("GPU detectada: {gpu_info:?}");
        let backend = gpu::resolve(cfg.gpu, &gpu_info);
        gpu::apply(backend);
        let e = Engine(Arc::new(Inner {
            cfg: Mutex::new(cfg),
            gpu_info,
            backend: Mutex::new(backend),
            gpu_note: Mutex::new(None),
            router: Arc::new(FrameRouter::default()),
            stats: Arc::new(OutStats::default()),
            output: Mutex::new(None),
            effects: Mutex::new(None),
            source: Mutex::new(None),
            switch: Mutex::new(()),
            gen: AtomicU64::new(0),
            source_state: Mutex::new("starting".into()),
            last_error: Mutex::new(None),
            recording: Arc::new(Mutex::new(None)),
            last_clip: Mutex::new(None),
            events: tx,
        }));
        e.spawn_supervisor(rx);
        Ok(e)
    }

    fn cfg(&self) -> Config {
        self.0.cfg.lock().unwrap().clone()
    }

    fn update_cfg(&self, f: impl FnOnce(&mut Config)) {
        let mut c = self.0.cfg.lock().unwrap();
        f(&mut c);
        c.save();
    }

    fn backend(&self) -> Backend {
        *self.0.backend.lock().unwrap()
    }

    fn set_error(&self, e: Option<String>) {
        if let Some(m) = &e {
            tracing::warn!("{m}");
        }
        *self.0.last_error.lock().unwrap() = e;
    }

    /// Sobe saída → efeitos → fonte salva na config (com fallback para padrão de teste).
    pub fn start(&self) -> Result<()> {
        let cfg = self.cfg();
        cfg.output.validate()?;
        *self.0.output.lock().unwrap() = Some(Output::start(&cfg.output, self.0.router.clone(), self.0.stats.clone())?);
        self.rebuild_effects()?;
        if let Err(e) = self.set_source(cfg.source.clone()) {
            self.set_error(Some(format!("fonte salva falhou ({e:#}); usando padrão de teste")));
            self.set_source(SourceSpec::Test { pattern: "smpte".into() })?;
        }
        Ok(())
    }

    // ---------- efeitos ----------

    fn rebuild_effects(&self) -> Result<()> {
        let cfg = self.cfg();
        let use_gl = effects::wants_gl(&cfg.effects, self.backend(), &self.0.gpu_info);
        let chain = match EffectsChain::build(&cfg.output, &cfg.effects, use_gl, self.0.router.clone(), self.0.events.clone()) {
            Ok(c) => c,
            Err(e) if use_gl => {
                *self.0.gpu_note.lock().unwrap() = Some(format!("filtro GL indisponível ({e:#}); efeitos na CPU"));
                EffectsChain::build(&cfg.output, &cfg.effects, false, self.0.router.clone(), self.0.events.clone())?
            }
            Err(e) => return Err(e),
        };
        self.0.router.set_fx(Some(chain.src.clone()));
        if let Some(old) = self.0.effects.lock().unwrap().replace(chain) {
            old.stop();
        }
        Ok(())
    }

    pub fn set_effects(&self, fx: Effects) -> Result<()> {
        let cfg = self.cfg();
        let want_gl = effects::wants_gl(&fx, self.backend(), &self.0.gpu_info);
        self.update_cfg(|c| c.effects = fx.clone());
        let live = {
            let guard = self.0.effects.lock().unwrap();
            match guard.as_ref() {
                Some(ch) if ch.gl == want_gl => {
                    ch.apply(&cfg.output, &fx);
                    true
                }
                _ => false,
            }
        };
        if !live {
            self.rebuild_effects()?;
        }
        Ok(())
    }

    // ---------- fonte ----------

    pub fn set_source(&self, spec: SourceSpec) -> Result<()> {
        let _g = self.0.switch.lock().unwrap();
        self.set_source_locked(spec)
    }

    fn set_source_locked(&self, spec: SourceSpec) -> Result<()> {
        let out = self.cfg().output;
        let gen = self.0.gen.fetch_add(1, Ordering::SeqCst) + 1;
        *self.0.source_state.lock().unwrap() = "starting".into();

        // A mesma câmera não abre duas vezes: libera a antiga antes (a saída segue com o último frame).
        if let SourceSpec::Camera { device, .. } = &spec {
            let mut cur = self.0.source.lock().unwrap();
            if cur.as_ref().is_some_and(|s| s.uses_device(device)) {
                cur.take().unwrap().stop_blocking();
            }
        }

        let build = || Source::build(&spec, &out, self.backend(), self.0.router.clone(), self.0.events.clone(), gen);
        let built = match build() {
            Ok(s) => Ok(s),
            Err(e) if self.backend() != Backend::Cpu => {
                self.fallback_to_cpu(&format!("{e:#}"));
                build()
            }
            Err(e) => Err(e),
        };

        match built {
            Ok(s) => {
                // Commit: a partir daqui só a nova fonte alimenta a saída.
                self.0.router.set_active(gen);
                if let Some(old) = self.0.source.lock().unwrap().replace(s) {
                    old.stop();
                }
                *self.0.source_state.lock().unwrap() = "playing".into();
                self.set_error(None);
                self.update_cfg(|c| c.source = spec);
                Ok(())
            }
            Err(e) => {
                let has_source = self.0.source.lock().unwrap().is_some();
                *self.0.source_state.lock().unwrap() = if has_source { "playing" } else { "error" }.into();
                self.set_error(Some(format!("{e:#}")));
                Err(e)
            }
        }
    }

    fn fallback_to_cpu(&self, reason: &str) {
        tracing::warn!("falha com GPU, voltando para CPU: {reason}");
        gpu::apply(Backend::Cpu);
        *self.0.backend.lock().unwrap() = Backend::Cpu;
        *self.0.gpu_note.lock().unwrap() = Some(format!("GPU falhou, usando CPU: {reason}"));
        let gl = self.0.effects.lock().unwrap().as_ref().is_some_and(|c| c.gl);
        if gl {
            let _ = self.rebuild_effects();
        }
    }

    fn with_source<T>(&self, f: impl FnOnce(&mut Source) -> Result<T>) -> Result<T> {
        let mut g = self.0.source.lock().unwrap();
        let s = g.as_mut().ok_or_else(|| anyhow!("nenhuma fonte ativa"))?;
        f(s)
    }

    // ---------- GPU ----------

    pub fn set_gpu(&self, mode: GpuMode) -> Result<()> {
        let _g = self.0.switch.lock().unwrap();
        let backend = gpu::resolve(mode, &self.0.gpu_info);
        gpu::apply(backend);
        *self.0.backend.lock().unwrap() = backend;
        *self.0.gpu_note.lock().unwrap() = match (mode, backend) {
            (GpuMode::Off, _) | (GpuMode::Auto, _) => None,
            (_, Backend::Cpu) => Some("GPU pedida não está disponível; usando CPU".into()),
            _ => None,
        };
        self.update_cfg(|c| c.gpu = mode);
        self.rebuild_effects()?;
        // Reconstrói só a fonte: a saída continua repetindo o último frame.
        let spec = self.0.source.lock().unwrap().as_ref().map(|s| s.spec.clone());
        if let Some(spec) = spec {
            self.set_source_locked(spec)?;
        }
        Ok(())
    }

    // ---------- saída ----------

    pub fn set_output(&self, o: OutputCfg) -> Result<()> {
        o.validate()?;
        let _g = self.0.switch.lock().unwrap();
        if let Some(old) = self.0.output.lock().unwrap().take() {
            old.stop();
        }
        if let Some(s) = self.0.source.lock().unwrap().take() {
            s.stop_blocking();
        }
        self.0.router.out.clear();
        self.0.router.raw.clear();
        self.update_cfg(|c| c.output = o.clone());
        *self.0.output.lock().unwrap() = Some(Output::start(&o, self.0.router.clone(), self.0.stats.clone())?);
        self.rebuild_effects()?;
        let spec = self.cfg().source;
        self.set_source_locked(spec)
    }

    // ---------- gravação ----------

    pub fn record(&self, secs: u32, then_loop: bool) -> Result<()> {
        anyhow::ensure!((1..=120).contains(&secs), "duração deve estar entre 1 e 120 s");
        anyhow::ensure!(self.0.recording.lock().unwrap().is_none(), "já existe uma gravação em andamento");
        self.record_with(secs, then_loop, self.backend())
    }

    fn record_with(&self, secs: u32, then_loop: bool, backend: Backend) -> Result<()> {
        recorder::spawn(
            secs,
            then_loop,
            self.cfg().output,
            backend,
            self.0.router.clone(),
            self.0.recording.clone(),
            self.0.events.clone(),
        )
    }

    // ---------- supervisor ----------

    fn spawn_supervisor(&self, rx: Receiver<Event>) {
        let me = self.clone();
        std::thread::Builder::new()
            .name("mockcam-sup".into())
            .spawn(move || {
                for ev in rx {
                    me.on_event(ev);
                }
            })
            .expect("thread supervisor");
    }

    fn on_event(&self, ev: Event) {
        let cur = self.0.gen.load(Ordering::SeqCst);
        match ev {
            Event::SourceError { gen, msg } if gen == cur => {
                if self.backend() != Backend::Cpu {
                    self.fallback_to_cpu(&msg);
                    let spec = self.0.source.lock().unwrap().as_ref().map(|s| s.spec.clone());
                    if let Some(spec) = spec {
                        let _ = self.set_source(spec);
                    }
                } else {
                    *self.0.source_state.lock().unwrap() = "error".into();
                    self.set_error(Some(msg));
                }
            }
            Event::SourceEos { gen } if gen == cur => {
                *self.0.source_state.lock().unwrap() = "ended".into();
            }
            Event::EffectsError { gl, msg } => {
                if gl {
                    *self.0.gpu_note.lock().unwrap() = Some(format!("filtro GL falhou, efeitos na CPU: {msg}"));
                    let fx = Effects { gl_filter: crate::GlFilter::None, ..self.cfg().effects };
                    let _ = self.set_effects(fx);
                } else {
                    self.set_error(Some(format!("efeitos: {msg}")));
                    // Sem efeitos é melhor que sem vídeo.
                    self.0.router.set_fx(None);
                }
            }
            Event::RecordDone { path, then_loop } => {
                *self.0.last_clip.lock().unwrap() = Some(path.clone());
                if then_loop {
                    let _ = self.set_source(SourceSpec::File { path, looped: true });
                }
            }
            Event::RecordFailed { msg, secs, then_loop, gpu_enc } => {
                if gpu_enc {
                    tracing::warn!("gravação com encoder de GPU falhou ({msg}); refazendo pela CPU");
                    *self.0.gpu_note.lock().unwrap() = Some(format!("encoder de GPU falhou, gravando pela CPU: {msg}"));
                    if let Err(e) = self.record_with(secs, then_loop, Backend::Cpu) {
                        self.set_error(Some(format!("gravação falhou: {e:#}")));
                    }
                } else {
                    self.set_error(Some(format!("gravação falhou: {msg}")));
                }
            }
            _ => {} // eventos de fontes antigas
        }
    }

    // ---------- status / IPC ----------

    pub fn status(&self) -> Status {
        let cfg = self.cfg();
        let (position, duration, speed, paused) = match self.0.source.lock().unwrap().as_ref() {
            Some(s) if s.spec.is_seekable() => (s.position(), s.duration(), s.rate(), s.paused),
            Some(s) => (None, None, 1.0, s.paused),
            None => (None, None, 1.0, false),
        };
        let mut state = self.0.source_state.lock().unwrap().clone();
        if paused && state == "playing" {
            state = "paused".into();
        }
        Status {
            source: self.0.source.lock().unwrap().as_ref().map(|s| s.spec.clone()),
            source_state: state,
            position,
            duration,
            speed,
            frozen: self.0.stats.frozen.load(Ordering::Relaxed),
            gpu_mode: cfg.gpu,
            backend: self.backend(),
            gpu: self.0.gpu_info,
            gl_active: self.0.effects.lock().unwrap().as_ref().is_some_and(|c| c.gl),
            gpu_note: self.0.gpu_note.lock().unwrap().clone(),
            effects: cfg.effects,
            device_ok: self.0.stats.device_ok.load(Ordering::Relaxed),
            output_error: self.0.stats.error.lock().unwrap().clone(),
            out_fps: self.0.stats.out_fps(),
            in_fps: self.0.stats.in_fps(),
            recording: *self.0.recording.lock().unwrap(),
            last_clip: self.0.last_clip.lock().unwrap().clone(),
            last_error: self.0.last_error.lock().unwrap().clone(),
            cameras: source::list_cameras(&cfg.output.device),
            output: cfg.output,
        }
    }

    /// Executa um comando. Retorna (resposta, deve_encerrar).
    pub fn handle(&self, req: Request) -> (Response, bool) {
        let mut exit = false;
        let r: Result<()> = match req {
            Request::Status => Ok(()),
            Request::SetSource { source } => self.set_source(source),
            Request::SetEffects { effects } => self.set_effects(effects),
            Request::SetGpu { mode } => self.set_gpu(mode),
            Request::SetOutput { output } => self.set_output(output),
            Request::Freeze { on } => {
                self.0.stats.frozen.store(on, Ordering::Relaxed);
                Ok(())
            }
            Request::Pause => self.with_source(|s| s.set_paused(true)),
            Request::Resume => self.with_source(|s| s.set_paused(false)),
            Request::Seek { secs } => self.with_source(|s| s.seek(secs)).map(|_| {
                *self.0.source_state.lock().unwrap() = "playing".into();
            }),
            Request::Speed { rate } => self.with_source(|s| s.set_rate(rate)),
            Request::Record { secs, then_loop } => self.record(secs, then_loop),
            Request::Shutdown => {
                exit = true;
                Ok(())
            }
        };
        let st = self.status();
        let resp = match r {
            Ok(()) => Response::ok(st),
            Err(e) => Response::err(format!("{e:#}"), Some(st)),
        };
        (resp, exit)
    }

    pub fn shutdown(&self) {
        let _g = self.0.switch.lock().unwrap();
        if let Some(s) = self.0.source.lock().unwrap().take() {
            s.stop_blocking();
        }
        if let Some(o) = self.0.output.lock().unwrap().take() {
            o.stop();
        }
    }
}
