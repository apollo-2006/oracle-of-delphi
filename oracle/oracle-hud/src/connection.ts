// WebSocket client with the "state, not history" backpressure discipline
// (architecture §6.1). Binary FFT/SYS frames are latest-wins; JSON agent events
// are queued and never dropped. All handlers only write to buffers; the render
// loop consumes them, decoupling network cadence from frame cadence.

import { AgentEvent, decodeFft, decodeHeader, FftFrame, FrameType } from "./protocol.js";

export interface HudBuffers {
  latestFft: FftFrame | null;
  agentEvents: AgentEvent[]; // drained by the app each frame
}

export class HudConnection {
  readonly buffers: HudBuffers = { latestFft: null, agentEvents: [] };
  private ws: WebSocket | null = null;
  private url: string;
  private reconnectMs = 500;

  constructor(url: string) {
    this.url = url;
  }

  connect(): void {
    const ws = new WebSocket(this.url);
    ws.binaryType = "arraybuffer";
    ws.onmessage = (ev) => this.onMessage(ev);
    ws.onclose = () => this.scheduleReconnect();
    ws.onerror = () => ws.close();
    this.ws = ws;
  }

  private onMessage(ev: MessageEvent): void {
    if (ev.data instanceof ArrayBuffer) {
      const view = new DataView(ev.data);
      const header = decodeHeader(view);
      if (header.type === FrameType.Fft) {
        // latest-wins: just overwrite; the render loop reads the newest.
        this.buffers.latestFft = decodeFft(ev.data);
      }
      // SYS frames could be decoded here similarly; omitted for brevity.
    } else if (typeof ev.data === "string") {
      try {
        const parsed = JSON.parse(ev.data) as AgentEvent;
        this.buffers.agentEvents.push(parsed); // never dropped
      } catch {
        /* ignore malformed */
      }
    }
  }

  /** Send a control command back to core (interrupt/mute/confirm). */
  send(obj: unknown): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(obj));
    }
  }

  private scheduleReconnect(): void {
    setTimeout(() => this.connect(), this.reconnectMs);
    this.reconnectMs = Math.min(this.reconnectMs * 2, 8000);
  }
}
