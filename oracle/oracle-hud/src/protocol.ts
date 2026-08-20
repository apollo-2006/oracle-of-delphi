// Binary telemetry protocol — mirrors oracle-ipc/src/audio.rs exactly.
// The 8-byte header { u8 type, u8 flags, u16 seq, u32 t_ms } precedes each frame.
// Keeping this in one place is what stops the Rust and TS sides from drifting.

export const FFT_BANDS = 64;

export enum FrameType {
  Fft = 1,
  Sys = 2,
  Agent = 3,
}

export const FLAG_SOURCE_MIC = 0b0000_0001;

export interface FrameHeader {
  type: FrameType;
  flags: number;
  seq: number;
  tMs: number;
}

export interface FftFrame {
  header: FrameHeader;
  bands: Float32Array; // length FFT_BANDS
  fromMic: boolean;
}

export interface SysFrame {
  gpuUtil: number;
  gpuTempC: number;
  vramMb: number;
  tokPerS: number;
  asrRtf: number;
}

/** Decode the shared 8-byte header from a DataView at offset 0. */
export function decodeHeader(view: DataView): FrameHeader {
  return {
    type: view.getUint8(0) as FrameType,
    flags: view.getUint8(1),
    seq: view.getUint16(2, /*littleEndian*/ true),
    tMs: view.getUint32(4, true),
  };
}

/** Decode a full FFT frame (header + 64 little-endian f32 bands). */
export function decodeFft(buffer: ArrayBuffer): FftFrame {
  const view = new DataView(buffer);
  const header = decodeHeader(view);
  const bands = new Float32Array(FFT_BANDS);
  for (let i = 0; i < FFT_BANDS; i++) {
    bands[i] = view.getFloat32(8 + i * 4, true);
  }
  return { header, bands, fromMic: (header.flags & FLAG_SOURCE_MIC) !== 0 };
}

/** Agent-side JSON events (the non-binary WebSocket channel). */
export type AgentEvent =
  | { type: "state"; turn: string; state: string }
  | { type: "transcript"; text: string; stable: boolean }
  | { type: "caption"; text: string }
  | { type: "tool"; id: number; name: string; status: ToolStatus; detail?: string }
  | { type: "sys"; gpu_util: number; gpu_temp_c: number; vram_mb: number; tok_per_s: number; asr_rtf: number }
  | { type: "confirm"; request_id: string; prompt: string; severity: string };

export type ToolStatus = "started" | "progress" | "done" | "error";

/** The visual mode the core is in, which drives the arc-core palette. */
export type HudState = "idle" | "listening" | "thinking" | "speaking" | "tool";

export function stateFromString(s: string): HudState {
  switch (s) {
    case "listening":
      return "listening";
    case "thinking":
    case "planning":
      return "thinking";
    case "speaking":
      return "speaking";
    case "tool":
    case "acting":
      return "tool";
    default:
      return "idle";
  }
}
