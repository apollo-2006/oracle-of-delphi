//! Binary telemetry frame layout shared with the HUD. Kept in the IPC crate so
//! the frame header stays in one place; the TS side mirrors this exactly.

/// Frame type discriminants for the binary WebSocket subprotocol.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Fft = 1,
    Sys = 2,
    Agent = 3,
}

impl FrameType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(FrameType::Fft),
            2 => Some(FrameType::Sys),
            3 => Some(FrameType::Agent),
            _ => None,
        }
    }
}

/// Flag bit: the active FFT source is the microphone (listening) vs TTS (speaking).
pub const FLAG_SOURCE_MIC: u8 = 0b0000_0001;

/// Number of log-spaced FFT bands sent per frame (50 Hz – 8 kHz).
pub const FFT_BANDS: usize = 64;

/// 8-byte little-endian header prefixed to every binary frame.
/// `{ u8 type, u8 flags, u16 seq, u32 t_ms }`
pub fn encode_header(buf: &mut Vec<u8>, ty: FrameType, flags: u8, seq: u16, t_ms: u32) {
    buf.push(ty as u8);
    buf.push(flags);
    buf.extend_from_slice(&seq.to_le_bytes());
    buf.extend_from_slice(&t_ms.to_le_bytes());
}

/// Encode a full FFT frame (header + 64 f32 little-endian bands).
pub fn encode_fft(bands: &[f32; FFT_BANDS], seq: u16, t_ms: u32, from_mic: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + FFT_BANDS * 4);
    let flags = if from_mic { FLAG_SOURCE_MIC } else { 0 };
    encode_header(&mut buf, FrameType::Fft, flags, seq, t_ms);
    for b in bands {
        buf.extend_from_slice(&b.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_frame_is_expected_size() {
        let bands = [0.5f32; FFT_BANDS];
        let f = encode_fft(&bands, 3, 1000, true);
        assert_eq!(f.len(), 8 + FFT_BANDS * 4);
        assert_eq!(f[0], FrameType::Fft as u8);
        assert_eq!(f[1] & FLAG_SOURCE_MIC, FLAG_SOURCE_MIC);
        // seq little-endian at offset 2
        assert_eq!(u16::from_le_bytes([f[2], f[3]]), 3);
    }
}
