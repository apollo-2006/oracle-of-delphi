//! HUD gateway (architecture §6.1): the binary telemetry encoders, the
//! latest-wins backpressure policy for FFT/SYS frames, and the live WebSocket
//! server ([`server::HudGateway`]) that authenticates clients and streams to
//! them.

pub mod server;

use oracle_ipc::audio::{encode_fft, FFT_BANDS};

/// Bundles the "state, not history" drop policy for telemetry. FFT/SYS frames
/// are coalesced: if the socket is backed up, we keep only the newest.
#[derive(Default)]
pub struct TelemetryCoalescer {
    latest_fft: Option<Vec<u8>>,
    latest_sys: Option<Vec<u8>>,
    seq: u16,
}

impl TelemetryCoalescer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_fft(&mut self, bands: &[f32; FFT_BANDS], t_ms: u32, from_mic: bool) {
        self.seq = self.seq.wrapping_add(1);
        self.latest_fft = Some(encode_fft(bands, self.seq, t_ms, from_mic));
    }

    pub fn push_sys(&mut self, frame: Vec<u8>) {
        self.latest_sys = Some(frame);
    }

    /// Drain whatever is pending (newest only). Called by the socket writer when
    /// `bufferedAmount` is low enough to send again.
    pub fn drain(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if let Some(f) = self.latest_fft.take() {
            out.push(f);
        }
        if let Some(s) = self.latest_sys.take() {
            out.push(s);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalescer_keeps_only_newest_fft() {
        let mut c = TelemetryCoalescer::new();
        let a = [0.1f32; FFT_BANDS];
        let b = [0.9f32; FFT_BANDS];
        c.push_fft(&a, 1, true);
        c.push_fft(&b, 2, true); // supersedes a without a drain in between
        let drained = c.drain();
        assert_eq!(drained.len(), 1); // only one FFT frame survived
                                      // and it is the newer one (seq==2)
        let f = &drained[0];
        assert_eq!(u16::from_le_bytes([f[2], f[3]]), 2);
    }

    #[test]
    fn drain_empties_state() {
        let mut c = TelemetryCoalescer::new();
        c.push_fft(&[0.0; FFT_BANDS], 1, false);
        assert_eq!(c.drain().len(), 1);
        assert!(c.drain().is_empty());
    }
}
