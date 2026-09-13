// A stand-in for oracle-core's HUD gateway, for the static GitHub Pages demo.
//
// The HUD is unmodified: it opens a WebSocket, and in the demo build that
// WebSocket is this class. It speaks the gateway's real protocol (the AgentEvent
// union in protocol.ts, plus binary FFT frames) and plays short scripted turns
// shaped like the real ones: state changes, tool rows, the Apollo confirmation
// for irreversible actions, a streamed caption and a spoken reply.
//
// Nothing here is a model. The replies are fixed strings chosen by keyword, and
// the page says so.

import { FFT_BANDS, FrameType } from "../protocol.js";

type Json = Record<string, unknown>;

interface Step {
  after: number; // ms after the previous step
  event?: Json;
  run?: (live: () => boolean) => void | Promise<void>;
}

const STATUS = "scripted demo · no model running · actd simulated";

export class ScriptedGateway {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  readyState = ScriptedGateway.CONNECTING;
  binaryType: BinaryType = "arraybuffer";
  onopen: ((ev: Event) => void) | null = null;
  onmessage: ((ev: MessageEvent) => void) | null = null;
  onclose: ((ev: CloseEvent) => void) | null = null;
  onerror: ((ev: Event) => void) | null = null;

  private turn = 0; // bumped by every new turn and by interrupt; stale turns stop
  private toolId = 0;
  private pendingConfirm: ((allow: boolean) => void) | null = null;
  private pendingRequestId: string | null = null;
  private speaking: number | null = null;
  private seq = 0;
  private statusTimer: number;

  constructor(_url: string) {
    setTimeout(() => {
      this.readyState = ScriptedGateway.OPEN;
      this.onopen?.(new Event("open"));
    }, 120);
    this.statusTimer = window.setInterval(() => this.emit({ type: "status", text: STATUS }), 3000);
  }

  close(): void {
    this.readyState = ScriptedGateway.CLOSED;
    clearInterval(this.statusTimer);
  }

  send(data: string): void {
    let msg: Json;
    try {
      msg = JSON.parse(data);
    } catch {
      return;
    }
    switch (msg.type) {
      case "hello":
        // Report the Whisper path as live, which is what makes the HUD leave the
        // browser's always-on wake listener off. The mic then does nothing until
        // someone presses it.
        this.emit({ type: "config", stt: true, tts: false, wake: false });
        this.emit({ type: "state", turn: "", state: "idle" });
        this.emit({ type: "status", text: STATUS });
        break;
      case "user_text":
        void this.runTurn(String(msg.text ?? ""));
        break;
      case "audio":
        void this.runTurn("", "Voice input needs the local speech-to-text process, which this page does not have. Type instead.");
        break;
      case "confirm":
        // Only the verdict for the request that is actually pending counts, so a
        // late click on an older decree cannot sanction a newer action.
        if (msg.request_id === this.pendingRequestId) {
          this.pendingConfirm?.(Boolean(msg.allow));
          this.pendingConfirm = null;
          this.pendingRequestId = null;
        }
        break;
      case "interrupt":
        this.turn++;
        this.pendingConfirm?.(false);
        this.pendingConfirm = null;
        this.stopSpeaking();
        this.emit({ type: "stop_audio" });
        this.emit({ type: "state", turn: "", state: "idle" });
        break;
      case "summon":
        this.emit({ type: "state", turn: "", state: "listening" });
        break;
      case "set_wake":
        this.emit({ type: "config", stt: true, tts: false, wake: false });
        break;
    }
  }

  // ------------------------------------------------------------------ turns

  private async runTurn(text: string, fixedReply?: string): Promise<void> {
    // A new message supersedes whatever turn is running, as it does in core: the
    // old reply stops streaming and a pending sanction lapses as forbidden.
    const mine = ++this.turn;
    const live = () => this.turn === mine;
    this.pendingConfirm?.(false);
    this.pendingConfirm = null;
    this.stopSpeaking();
    this.toolId = 0;
    const turn = String(mine);
    if (text) this.emit({ type: "transcript", text, stable: true });
    this.emit({ type: "state", turn, state: "thinking" });

    const script = fixedReply ? [this.reply(fixedReply, turn)] : this.plan(text, turn);
    for (const step of script.flat()) {
      await sleep(step.after);
      if (!live()) return;
      if (step.event) this.emit(step.event);
      if (step.run) await step.run(live);
      if (!live()) return;
    }
    this.emit({ type: "state", turn, state: "idle" });
  }

  private plan(text: string, turn: string): Step[][] {
    const t = text.toLowerCase();
    const app = (t.match(/\b(chrome|firefox|spotify|discord|steam|obs|code|slack)\b/) ?? [])[1];

    if (/\b(kill|close|quit|terminate|end)\b/.test(t)) {
      const name = app ?? "chrome";
      return [this.tool(turn, "os.list_processes", 600, `${name}.exe found, pid 18244`),
        this.irreversible(turn, "os.kill_process", `Terminate ${name}.exe (pid 18244)?`,
          `Done. ${cap(name)} is closed.`, `Understood. I left ${cap(name)} running.`)];
    }
    if (/\b(rm|delete|shell|command|run)\b/.test(t)) {
      return [this.irreversible(turn, "os.shell", "Run `rm -rf ./build` in C:\\dev\\oracle?",
        "The build directory is gone.", "Nothing was deleted.")];
    }
    if (/\b(remember|note|don't forget)\b/.test(t)) {
      const fact = text.replace(/^.*?\b(remember|note)( that)?\b/i, "").trim() || text;
      return [this.tool(turn, "memory.remember", 500, "stored, episodic + vector"),
        this.reply(`I'll remember that ${fact.replace(/[.?!]+$/, "")}.`, turn)];
    }
    if (/\b(what did|recall|remind me|what do you know)\b/.test(t)) {
      return [this.tool(turn, "memory.recall", 700, "3 memories, best match 0.82"),
        this.reply("Last week you said your systems exam is on Friday, and that you wanted a reminder the night before.", turn)];
    }
    if (/\b(open|launch|start)\b/.test(t)) {
      const name = app ?? "spotify";
      return [this.tool(turn, "os.launch_app", 800, `${name} started`),
        this.reply(`Opening ${cap(name)}.`, turn)];
    }
    if (/\b(process|gpu|memory|ram|slow|using|cpu)\b/.test(t)) {
      return [this.tool(turn, "os.list_processes", 900, "212 processes"),
        this.reply("The heaviest right now are llama-server at 11.8 GB, Chrome at 2.1 GB across its tabs, and Steam at 640 MB. The GPU is mostly the model.", turn)];
    }
    if (/\b(search|look up|find)\b/.test(t)) {
      const q = text.replace(/^.*?\b(search( for)?|look up|find)\b/i, "").trim() || "Raft consensus";
      return [this.tool(turn, "os.web_search", 1200, `results for "${q}"`),
        this.reply(`I opened a search for ${q} in your browser.`, turn)];
    }
    if (/\b(lock)\b/.test(t)) {
      return [this.tool(turn, "os.lock_screen", 500, "locked"), this.reply("Locked.", turn)];
    }
    return [this.reply(
      "This page is a scripted demo, so I can only do a few things here. Try asking me to close Chrome, what is using your memory, to remember something, or to open Spotify.",
      turn,
    )];
  }

  private tool(turn: string, name: string, ms: number, detail: string): Step[] {
    const id = ++this.toolId;
    return [
      { after: 450, event: { type: "state", turn, state: "tool" } },
      { after: 0, event: { type: "tool", id, name, status: "started" } },
      { after: ms, event: { type: "tool", id, name, status: "done", detail } },
      { after: 200, event: { type: "state", turn, state: "thinking" } },
    ];
  }

  // An action the daemon parks until the user decides, as in os_tools.rs: the
  // tool starts, the Apollo modal asks, and the answer finishes it either way.
  private irreversible(turn: string, name: string, prompt: string, yes: string, no: string): Step[] {
    const id = ++this.toolId;
    let allowed = false;
    return [
      { after: 450, event: { type: "state", turn, state: "tool" } },
      { after: 0, event: { type: "tool", id, name, status: "started", detail: "awaiting sanction" } },
      {
        after: 300,
        run: () =>
          new Promise<void>((resolve) => {
            const requestId = crypto.randomUUID?.() ?? String(Date.now());
            this.pendingRequestId = requestId;
            this.pendingConfirm = (allow) => {
              allowed = allow;
              resolve();
            };
            this.emit({ type: "confirm", request_id: requestId, prompt, severity: "irreversible" });
          }),
      },
      {
        after: 400,
        run: () => {
          this.emit(allowed
            ? { type: "tool", id, name, status: "done", detail: "sanctioned" }
            : { type: "tool", id, name, status: "error", detail: "forbidden" });
        },
      },
      { after: 0, run: (live) => this.streamReply(allowed ? yes : no, turn, live) },
    ];
  }

  private reply(text: string, turn: string): Step[] {
    return [{ after: 500, run: (live) => this.streamReply(text, turn, live) }];
  }

  private async streamReply(text: string, turn: string, live: () => boolean): Promise<void> {
    this.emit({ type: "state", turn, state: "speaking" });
    this.emit({ type: "speak", text });
    this.startSpeaking(text.length);
    const words = text.split(" ");
    for (let i = 1; i <= words.length; i++) {
      if (!live()) return;
      this.emit({ type: "caption", text: words.slice(0, i).join(" ") });
      await sleep(40);
    }
    await sleep(Math.min(2500, text.length * 20));
    if (live()) this.stopSpeaking();
  }

  // ------------------------------------------------------------ audio frames

  // Synthetic FFT frames in the binary layout protocol.ts decodes, so the arc
  // core reacts while "speaking". The real ones come from oracle-audio.
  private startSpeaking(chars: number): void {
    this.stopSpeaking();
    const start = performance.now();
    this.speaking = window.setInterval(() => {
      const t = (performance.now() - start) / 1000;
      const buf = new ArrayBuffer(8 + FFT_BANDS * 4);
      const view = new DataView(buf);
      view.setUint8(0, FrameType.Fft);
      view.setUint8(1, 0);
      view.setUint16(2, this.seq++ & 0xffff, true);
      view.setUint32(4, Math.floor(t * 1000), true);
      const syllable = 0.55 + 0.45 * Math.abs(Math.sin(t * 9.5)) * (0.7 + 0.3 * Math.sin(t * 2.3));
      for (let i = 0; i < FFT_BANDS; i++) {
        const formant = Math.exp(-((i - 10) ** 2) / 40) + 0.6 * Math.exp(-((i - 26) ** 2) / 90);
        view.setFloat32(8 + i * 4, syllable * formant * (0.8 + 0.2 * Math.random()), true);
      }
      this.onmessage?.(new MessageEvent("message", { data: buf }));
      if (t > Math.min(8, 1 + chars * 0.06)) this.stopSpeaking();
    }, 33);
  }

  private stopSpeaking(): void {
    if (this.speaking !== null) {
      clearInterval(this.speaking);
      this.speaking = null;
      const buf = new ArrayBuffer(8 + FFT_BANDS * 4);
      new DataView(buf).setUint8(0, FrameType.Fft);
      this.onmessage?.(new MessageEvent("message", { data: buf }));
    }
  }

  private emit(event: Json): void {
    if (this.readyState !== ScriptedGateway.OPEN) return;
    this.onmessage?.(new MessageEvent("message", { data: JSON.stringify(event) }));
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

function cap(s: string): string {
  return s === "obs" ? "OBS" : s === "code" ? "VS Code" : s[0].toUpperCase() + s.slice(1);
}
