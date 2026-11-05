//! Comunicação com o daemon fora da thread de UI (a UI nunca bloqueia).

use eframe::egui;
use mockcam_core::ipc::{self, Request, Status};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub struct Client {
    pub status: Arc<Mutex<Option<Status>>>,
    pub error: Arc<Mutex<Option<String>>>,
    pub connected: Arc<AtomicBool>,
    tx: Sender<Request>,
}

impl Client {
    pub fn new(ctx: egui::Context) -> Client {
        let (tx, rx) = channel::<Request>();
        let c = Client {
            status: Default::default(),
            error: Default::default(),
            connected: Default::default(),
            tx,
        };
        let me = c.clone();
        std::thread::Builder::new()
            .name("mockcam-client".into())
            .spawn(move || loop {
                let first = match rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(r) => r,
                    Err(RecvTimeoutError::Timeout) => Request::Status,
                    Err(RecvTimeoutError::Disconnected) => return,
                };
                // Coalesce: durante um arrasto de slider só o último SetEffects importa.
                let mut batch = vec![first];
                batch.extend(rx.try_iter());
                let last_fx = batch.iter().rposition(|r| matches!(r, Request::SetEffects { .. }));
                for (i, req) in batch.into_iter().enumerate() {
                    if matches!(req, Request::SetEffects { .. }) && Some(i) != last_fx {
                        continue;
                    }
                    let is_cmd = !matches!(req, Request::Status);
                    match ipc::request(&req) {
                        Ok(resp) => {
                            me.connected.store(true, Ordering::Relaxed);
                            if let Some(st) = resp.status {
                                *me.status.lock().unwrap() = Some(st);
                            }
                            if is_cmd {
                                *me.error.lock().unwrap() = resp.error;
                            }
                        }
                        Err(e) => {
                            me.connected.store(false, Ordering::Relaxed);
                            *me.status.lock().unwrap() = None;
                            if is_cmd {
                                *me.error.lock().unwrap() = Some(format!("{e:#}"));
                            }
                        }
                    }
                }
                ctx.request_repaint();
            })
            .expect("thread do cliente");
        c
    }

    pub fn send(&self, r: Request) {
        let _ = self.tx.send(r);
    }
}
