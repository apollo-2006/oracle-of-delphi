// Microphone capture for local Whisper transcription.
//
// The browser's Web Speech API is unreliable inside the native WebView, so when
// server-side STT is on we capture raw audio ourselves and hand it to core to
// transcribe. We record via the Web Audio API (which works in WebView2 once the
// shell grants mic access), downsample to the 16 kHz mono PCM Whisper expects,
// and encode a WAV the backend can read directly — no webm/ffmpeg round-trip.

export class Recorder {
  private ctx: AudioContext | null = null;
  private stream: MediaStream | null = null;
  private processor: ScriptProcessorNode | null = null;
  private source: MediaStreamAudioSourceNode | null = null;
  private chunks: Float32Array[] = [];
  private inRate = 48000;
  recording = false;

  /** Begin capturing. Throws if mic access is denied. */
  async start(): Promise<void> {
    this.stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const Ctx: typeof AudioContext =
      window.AudioContext ??
      (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
    this.ctx = new Ctx();
    this.inRate = this.ctx.sampleRate;
    this.source = this.ctx.createMediaStreamSource(this.stream);
    // ScriptProcessor is deprecated but universally supported in WebView2 and
    // needs no extra worklet file. 4096-frame buffer, mono in/out.
    this.processor = this.ctx.createScriptProcessor(4096, 1, 1);
    this.chunks = [];
    this.processor.onaudioprocess = (e: AudioProcessingEvent) => {
      const data = e.inputBuffer.getChannelData(0);
      this.chunks.push(new Float32Array(data)); // copy — the buffer is reused
    };
    this.source.connect(this.processor);
    this.processor.connect(this.ctx.destination);
    this.recording = true;
  }

  /** Stop capturing and return a base64 16 kHz mono WAV, or null if silent. */
  async stop(): Promise<string | null> {
    this.recording = false;
    if (this.processor) {
      this.processor.disconnect();
      this.processor.onaudioprocess = null;
    }
    this.source?.disconnect();
    this.stream?.getTracks().forEach((t) => t.stop());
    const ctx = this.ctx;
    this.ctx = null;
    this.source = null;
    this.processor = null;
    this.stream = null;

    const total = this.chunks.reduce((n, c) => n + c.length, 0);
    if (ctx) {
      try {
        await ctx.close();
      } catch {
        /* already closed */
      }
    }
    if (total === 0) {
      this.chunks = [];
      return null;
    }
    const flat = new Float32Array(total);
    let off = 0;
    for (const c of this.chunks) {
      flat.set(c, off);
      off += c.length;
    }
    this.chunks = [];
    const down = downsample(flat, this.inRate, 16000);
    return wavBase64(down, 16000);
  }
}

/** Average-decimate a mono signal from `inRate` to `outRate` (outRate < inRate). */
function downsample(input: Float32Array, inRate: number, outRate: number): Float32Array {
  if (outRate >= inRate) return input;
  const ratio = inRate / outRate;
  const outLen = Math.floor(input.length / ratio);
  const out = new Float32Array(outLen);
  for (let i = 0; i < outLen; i++) {
    const start = Math.floor(i * ratio);
    const end = Math.floor((i + 1) * ratio);
    let sum = 0;
    let cnt = 0;
    for (let j = start; j < end && j < input.length; j++) {
      sum += input[j];
      cnt++;
    }
    out[i] = cnt ? sum / cnt : 0;
  }
  return out;
}

/** Encode 16-bit PCM WAV from float samples and base64 it. */
function wavBase64(samples: Float32Array, rate: number): string {
  const buf = new ArrayBuffer(44 + samples.length * 2);
  const view = new DataView(buf);
  const writeStr = (o: number, s: string) => {
    for (let i = 0; i < s.length; i++) view.setUint8(o + i, s.charCodeAt(i));
  };
  writeStr(0, "RIFF");
  view.setUint32(4, 36 + samples.length * 2, true);
  writeStr(8, "WAVE");
  writeStr(12, "fmt ");
  view.setUint32(16, 16, true); // PCM chunk size
  view.setUint16(20, 1, true); // format = PCM
  view.setUint16(22, 1, true); // channels = 1
  view.setUint32(24, rate, true);
  view.setUint32(28, rate * 2, true); // byte rate
  view.setUint16(32, 2, true); // block align
  view.setUint16(34, 16, true); // bits per sample
  writeStr(36, "data");
  view.setUint32(40, samples.length * 2, true);
  let o = 44;
  for (let i = 0; i < samples.length; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    view.setInt16(o, s < 0 ? s * 0x8000 : s * 0x7fff, true);
    o += 2;
  }
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin);
}
