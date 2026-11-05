//! Núcleo do MockCam: pipelines GStreamer, roteamento de frames, GPU, config e IPC.
//!
//! Fluxo de dados (cada seta é só um ref-count de `gst::Buffer`, sem cópia):
//!
//! ```text
//! [Source pipeline] --appsink--> FrameRouter --+--> raw slot (gravação)
//!                                              +--> [Effects pipeline] --appsink--> out slot
//! [Output thread, fps fixo] <-- out slot  --> appsrc ! v4l2sink (/dev/video10)
//! ```
//!
//! Source e Effects são descartáveis (reconstruídos ao trocar fonte/GPU/filtro);
//! a saída nunca para: se nada novo chegou, repete o último frame.

pub mod config;
pub mod effects;
pub mod engine;
pub mod gpu;
pub mod ipc;
pub mod output;
pub mod recorder;
pub mod slot;
pub mod source;
mod util;

pub use config::*;
pub use engine::Engine;
