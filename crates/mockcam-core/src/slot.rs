use gst_app::AppSrc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Guarda apenas o frame mais recente. Escrita/leitura O(1), sem fila:
/// o produtor nunca bloqueia e o consumidor nunca acumula latência.
#[derive(Default)]
pub struct FrameSlot {
    buf: Mutex<Option<gst::Buffer>>,
    count: AtomicU64,
}

impl FrameSlot {
    pub fn put(&self, b: gst::Buffer) {
        *self.buf.lock().unwrap() = Some(b);
        self.count.fetch_add(1, Ordering::Relaxed);
    }
    pub fn get(&self) -> Option<gst::Buffer> {
        self.buf.lock().unwrap().clone()
    }
    pub fn clear(&self) {
        *self.buf.lock().unwrap() = None;
    }
    /// Total de frames recebidos (para medir fps de entrada).
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

/// Recebe frames da fonte e os encaminha para o pipeline de efeitos
/// (ou direto para a saída, se não houver efeitos ativos).
#[derive(Default)]
pub struct FrameRouter {
    /// Frames crus da fonte (pré-efeitos) — usados pela gravação do loop.
    pub raw: FrameSlot,
    /// Frames finais que a saída envia ao dispositivo.
    pub out: FrameSlot,
    fx: Mutex<Option<AppSrc>>,
    /// Geração da fonte "no ar". Frames de outras gerações (a fonte antiga
    /// ainda parando, ou a nova ainda em validação) são descartados, para
    /// não intercalar imagens durante a troca.
    active_gen: AtomicU64,
}

impl FrameRouter {
    pub fn set_active(&self, gen: u64) {
        self.active_gen.store(gen, Ordering::SeqCst);
    }

    pub fn push_raw(&self, gen: u64, b: gst::Buffer) {
        if gen != self.active_gen.load(Ordering::Relaxed) {
            return;
        }
        self.raw.put(b.clone());
        let fx = self.fx.lock().unwrap().clone();
        match fx {
            Some(src) => {
                // appsrc com leaky-type=downstream: nunca bloqueia a fonte.
                let _ = src.push_buffer(b);
            }
            None => self.out.put(b),
        }
    }

    pub fn set_fx(&self, src: Option<AppSrc>) {
        *self.fx.lock().unwrap() = src;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_keeps_latest() {
        gst::init().unwrap();
        let s = FrameSlot::default();
        assert!(s.get().is_none());
        s.put(gst::Buffer::from_slice(vec![1u8]));
        s.put(gst::Buffer::from_slice(vec![2u8]));
        let b = s.get().unwrap();
        assert_eq!(b.map_readable().unwrap().as_slice(), &[2u8]);
        assert_eq!(s.count(), 2);
    }

    #[test]
    fn router_without_fx_goes_to_out() {
        gst::init().unwrap();
        let r = FrameRouter::default();
        r.push_raw(0, gst::Buffer::from_slice(vec![7u8]));
        r.push_raw(1, gst::Buffer::from_slice(vec![8u8])); // geração inativa: descartado
        assert_eq!(r.raw.count(), 1);
        assert_eq!(r.out.count(), 1);
    }
}
