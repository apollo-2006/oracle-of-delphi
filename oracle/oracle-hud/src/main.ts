// HUD entry point (architecture §6.3): scene, camera, the multi-pass
// post-processing chain (bloom → combined CA/scanline/vignette), the glass DOM
// panels, and the single rAF loop that consumes network buffers. Adaptive
// degradation drops quality when frame-time p95 slips.

import * as THREE from "three";
import { EffectComposer } from "three/addons/postprocessing/EffectComposer.js";
import { RenderPass } from "three/addons/postprocessing/RenderPass.js";
import { UnrealBloomPass } from "three/addons/postprocessing/UnrealBloomPass.js";
import { ShaderPass } from "three/addons/postprocessing/ShaderPass.js";
import { ArcCore } from "./arcCore.js";
import { HudConnection } from "./connection.js";
import { AgentEvent, stateFromString } from "./protocol.js";
import { VoiceLoop } from "./voice.js";
import { ApolloModal } from "./apolloModal.js";
import { Recorder } from "./recorder.js";

// --- Combined post FX: chromatic aberration + scanlines + vignette in ONE pass
const CombinedFXShader = {
  uniforms: {
    tDiffuse: { value: null as THREE.Texture | null },
    uAberration: { value: 0.0018 },
    uScanline: { value: 0.06 },
    uVignette: { value: 0.25 },
    uTime: { value: 0 },
  },
  vertexShader: /* glsl */ `
    varying vec2 vUv;
    void main() { vUv = uv; gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0); }
  `,
  fragmentShader: /* glsl */ `
    uniform sampler2D tDiffuse;
    uniform float uAberration;
    uniform float uScanline;
    uniform float uVignette;
    uniform float uTime;
    varying vec2 vUv;
    void main() {
      vec2 dir = vUv - 0.5;
      // chromatic aberration scaled by distance from center
      float r = texture2D(tDiffuse, vUv - dir * uAberration).r;
      float g = texture2D(tDiffuse, vUv).g;
      float b = texture2D(tDiffuse, vUv + dir * uAberration).b;
      vec3 col = vec3(r, g, b);
      // scanlines
      float scan = sin(vUv.y * 800.0 + uTime * 2.0) * 0.5 + 0.5;
      col *= 1.0 - uScanline * scan;
      // vignette
      float vig = smoothstep(0.8, 0.2, length(dir));
      col *= mix(1.0 - uVignette, 1.0, vig);
      gl_FragColor = vec4(col, 1.0);
    }
  `,
};

class Hud {
  private renderer: THREE.WebGLRenderer;
  private scene: THREE.Scene;
  private camera: THREE.PerspectiveCamera;
  private composer: EffectComposer;
  private bloom: UnrealBloomPass;
  private fxPass: ShaderPass;
  private core: ArcCore;
  private conn: HudConnection;
  private clock = new THREE.Clock();
  private resolutionScale = 0.9;
  private frameTimes: number[] = [];
  private voice: VoiceLoop;
  private apollo: ApolloModal;
  // The currently-playing neural-voice clip, tracked so barge-in / mute can cut
  // it off mid-sentence.
  private currentAudio: HTMLAudioElement | null = null;
  // The assistant bubble currently being streamed into (null between turns).
  private currentReplyEl: HTMLElement | null = null;
  // Activity-panel bookkeeping: turnSeq namespaces per-turn tool ids (which
  // reset each turn) so STARTED/DONE for the same tool land on ONE row;
  // toolSeq is the session-monotonic number shown to the user.
  private turnSeq = 0;
  private toolSeq = 0;
  private toolRows = new Map<string, HTMLElement>();
  // Whisper capture (used for the mic when server-side STT is active).
  private recorder = new Recorder();
  private sttEnabled = false;
  private configSeen = false;
  // Wake-word chip state (server-side Whisper listener).
  private wakeChip: HTMLButtonElement | null = null;
  private wakeActive = false;
  // Speak replies aloud. On by default — this is a voice assistant — and
  // independent of whether the mic is currently listening (so typed questions
  // are answered out loud too). Toggle with the 🔊 button.
  private voiceOut = true;

  constructor(canvas: HTMLCanvasElement, wsUrl: string) {
    this.renderer = new THREE.WebGLRenderer({ canvas, antialias: false, alpha: true });
    this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2) * this.resolutionScale);
    this.renderer.setSize(window.innerWidth, window.innerHeight);

    this.scene = new THREE.Scene();
    this.camera = new THREE.PerspectiveCamera(50, window.innerWidth / window.innerHeight, 0.1, 100);
    this.camera.position.set(0, 0, 8);

    this.core = new ArcCore();
    this.scene.add(this.core.object);

    // Post chain: render → bloom (half-res) → combined FX.
    this.composer = new EffectComposer(this.renderer);
    this.composer.addPass(new RenderPass(this.scene, this.camera));
    this.bloom = new UnrealBloomPass(
      new THREE.Vector2(window.innerWidth, window.innerHeight),
      0.9, // strength
      0.6, // radius
      0.85, // threshold
    );
    this.composer.addPass(this.bloom);
    this.fxPass = new ShaderPass(CombinedFXShader);
    this.fxPass.renderToScreen = true;
    this.composer.addPass(this.fxPass);

    this.conn = new HudConnection(wsUrl);
    this.conn.connect();

    // Voice loop: recognized speech becomes a user_text message; the user
    // talking over a reply is a barge-in (cancel TTS + interrupt the turn).
    this.voice = new VoiceLoop({
      onUtterance: (text) => {
        // Core echoes the message back as a transcript event, which is what
        // appends the user bubble — so we don't add one here (avoids doubles).
        this.conn.send({ type: "user_text", text });
        setHint("");
      },
      onPartial: (text) => setHint("🎙 " + text + " …"),
      onBargeIn: () => {
        this.stopAudio();
        this.conn.send({ type: "interrupt" });
      },
      onStatus: (msg) => setHint("🎙 " + msg),
      onWake: (command) => {
        // Wake word heard — raise the window (core relays to the shell) and
        // show we're attending. A bare "Delphi" leaves the mic open briefly for
        // the follow-up; "Delphi, do X" already forwarded X as the utterance.
        this.conn.send({ type: "summon" });
        setState("listening");
        setHint(command ? "" : "Yes? I'm listening…");
      },
    });

    // The Apollo decree modal: the user's verdict on irreversible actions goes
    // straight back to core as a Confirm command.
    this.apollo = new ApolloModal((requestId, allow) => {
      this.conn.send({ type: "confirm", request_id: requestId, allow });
    });

    window.addEventListener("resize", () => this.onResize());
    this.wireControls();
  }

  private wireControls(): void {
    const interrupt = document.getElementById("interrupt");
    interrupt?.addEventListener("click", () => {
      this.stopSpeech();
      this.conn.send({ type: "interrupt" });
    });

    const input = document.getElementById("userInput") as HTMLInputElement | null;
    input?.addEventListener("keydown", (e: KeyboardEvent) => {
      if (e.key === "Enter" && input.value.trim().length > 0) {
        const text = input.value.trim();
        // Core echoes it back as a transcript event → that appends the user
        // bubble, so we just send and clear the box.
        this.conn.send({ type: "user_text", text });
        setHint("");
        input.value = "";
      }
    });

    const mic = document.getElementById("mic") as HTMLButtonElement | null;
    if (mic) {
      if (!this.voice.supported) {
        mic.disabled = true;
        mic.title = "Voice needs a Chromium-based browser (Web Speech API)";
      }
      mic.addEventListener("click", () => {
        if (this.sttEnabled) {
          void this.toggleRecording(mic);
          return;
        }
        // Not in Whisper mode. Say why, so a missing config is visible rather
        // than silently falling back to the flaky browser recognizer.
        if (!this.configSeen) {
          setHint("no capabilities from core yet — is this build's core running?");
        } else {
          setHint("Whisper off (set [voice] stt_enabled + stt_program) — using browser mic");
        }
        const on = this.voice.toggle();
        mic.classList.toggle("active", on);
        mic.textContent = on ? "🎙 Listening…" : "🎙 Voice";
      });
    }

    // Wake-word chip ("Delphi"). Two backends share it: with Whisper on, it
    // toggles the server-side streaming listener; otherwise it arms the browser
    // wake loop. The mode is decided by the config event from core.
    const wake = document.getElementById("wake") as HTMLButtonElement | null;
    if (wake) {
      this.wakeChip = wake;
      wake.addEventListener("click", () => {
        if (this.sttEnabled) {
          this.wakeActive = !this.wakeActive;
          this.conn.send({ type: "set_wake", active: this.wakeActive });
          this.refreshWakeChip();
        } else if (this.voice.supported) {
          this.voice.enableWake(!this.voice.isWakeOn);
          this.refreshWakeChip();
        }
      });
      // Until config says otherwise, arm the browser wake loop as before.
      if (this.voice.supported) {
        this.voice.enableWake(true);
        const rearm = () => {
          if (!this.sttEnabled && this.voice.isWakeOn) this.voice.enableWake(true);
          this.refreshWakeChip();
          window.removeEventListener("pointerdown", rearm);
          window.removeEventListener("keydown", rearm);
        };
        window.addEventListener("pointerdown", rearm);
        window.addEventListener("keydown", rearm);
      }
      this.refreshWakeChip();
    }

    // Spoken-reply toggle. On by default; lets the user silence TTS.
    const speak = document.getElementById("speak") as HTMLButtonElement | null;
    if (speak) {
      speak.classList.toggle("active", this.voiceOut);
      speak.addEventListener("click", () => {
        this.voiceOut = !this.voiceOut;
        speak.classList.toggle("active", this.voiceOut);
        speak.textContent = this.voiceOut ? "🔊 Voice reply" : "🔇 Muted";
        if (!this.voiceOut) this.stopSpeech();
      });
    }
  }

  private onResize(): void {
    this.camera.aspect = window.innerWidth / window.innerHeight;
    this.camera.updateProjectionMatrix();
    this.renderer.setSize(window.innerWidth, window.innerHeight);
    this.composer.setSize(window.innerWidth, window.innerHeight);
  }

  private drainEvents(): void {
    const evts = this.conn.buffers.agentEvents;
    while (evts.length) {
      const ev = evts.shift() as AgentEvent;
      this.applyEvent(ev);
    }
    const fft = this.conn.buffers.latestFft;
    if (fft) this.core.setBands(fft.bands);
  }

  private applyEvent(ev: AgentEvent): void {
    switch (ev.type) {
      case "state":
        this.core.setState(stateFromString(ev.state));
        setState(ev.state);
        break;
      case "transcript":
        // A finished user message → append a user bubble and start a fresh
        // assistant bubble for the reply that follows. (Interim partials come
        // from the local voice loop via the hint line, not from here.)
        if (ev.stable) {
          pushMessage("user", ev.text);
          this.currentReplyEl = null;
          this.turnSeq++; // new turn → new namespace for tool ids
          setHint("");
        }
        break;
      case "caption":
        // Caption streams the growing reply into the live assistant bubble
        // (visual only; speech comes via the "speak" event).
        this.currentReplyEl = streamReply(this.currentReplyEl, ev.text);
        break;
      case "speak":
        this.handleSpeak(ev.text, ev.wav_b64 ?? undefined);
        break;
      case "config":
        // Core told us which input path is live. With Whisper on, it owns voice
        // input: silence the browser wake listener (mic contention) and turn the
        // mic button into push-to-talk.
        console.log("[hud] config received:", ev);
        this.configSeen = true;
        this.sttEnabled = ev.stt;
        this.wakeActive = ev.wake ?? false;
        if (ev.stt) {
          // Whisper owns the mic — stop the browser wake loop (contention). The
          // wake chip now reflects/controls the server-side listener.
          this.voice.enableWake(false);
          const mic = document.getElementById("mic");
          if (mic) mic.textContent = "🎙 Voice";
        }
        this.refreshWakeChip();
        break;
      case "interim":
        // Live "what I'm hearing" from the always-on wake listener.
        setHint("🎙 " + ev.text);
        break;
      case "stop_audio":
        // Barge-in from the wake word — cut off her current speech.
        this.stopSpeech();
        break;
      case "tool":
        this.upsertTool(ev.id, ev.name, ev.status, ev.detail);
        break;
      case "sys":
        setText(
          "sys",
          `GPU ${ev.gpu_util.toFixed(0)}% ${ev.gpu_temp_c.toFixed(0)}°C · VRAM ${ev.vram_mb}MB · ${ev.tok_per_s.toFixed(0)} tok/s · RTF ${ev.asr_rtf.toFixed(2)}`,
        );
        break;
      case "status":
        // Core-composed status line for the System panel (model · backend ·
        // actd · throughput).
        setText("sys", ev.text);
        break;
      case "confirm":
        this.apollo.show({
          requestId: ev.request_id,
          prompt: ev.prompt,
          severity: ev.severity,
        });
        break;
    }
  }

  // Voice a reply. Prefer the neural WAV core synthesized; fall back to the
  // browser's speech engine when core sent none. Honors the mute toggle.
  private handleSpeak(text: string, wavB64?: string): void {
    if (!this.voiceOut) return; // muted
    if (wavB64 && wavB64.length > 0) {
      this.playWav(wavB64);
    } else if (text.trim().length > 0) {
      this.voice.speak(text);
    }
  }

  // Decode a base64 WAV and play it, replacing any clip already playing.
  private playWav(b64: string): void {
    this.stopAudio();
    try {
      const bin = atob(b64);
      const buf = new Uint8Array(bin.length);
      for (let i = 0; i < bin.length; i++) buf[i] = bin.charCodeAt(i);
      const url = URL.createObjectURL(new Blob([buf], { type: "audio/wav" }));
      const audio = new Audio(url);
      audio.onended = () => {
        URL.revokeObjectURL(url);
        if (this.currentAudio === audio) this.currentAudio = null;
      };
      this.currentAudio = audio;
      audio.play().catch((e) => console.warn("[audio] play failed", e));
    } catch (e) {
      console.warn("[audio] decode failed", e);
    }
  }

  // Cut off whatever the Oracle is currently saying — both the neural clip and
  // any browser speech — for barge-in, Stop, and mute.
  private stopSpeech(): void {
    this.stopAudio();
    this.voice.cancelSpeak();
  }

  private stopAudio(): void {
    if (this.currentAudio) {
      this.currentAudio.pause();
      this.currentAudio = null;
    }
  }

  // Reflect the wake chip from whichever backend is active: the server-side
  // Whisper listener (STT mode) or the browser wake loop.
  private refreshWakeChip(): void {
    const wake = this.wakeChip;
    if (!wake) return;
    const on = this.sttEnabled ? this.wakeActive : this.voice.isWakeOn;
    wake.disabled = false;
    wake.hidden = false;
    wake.classList.toggle("on", on);
    wake.textContent = on ? "◉ Delphi" : "○ Wake off";
    wake.title = on
      ? 'Listening for "Delphi" — click to stop'
      : 'Say "Delphi" to summon — click to start';
  }

  // Push-to-talk for Whisper: first click starts capturing, second click stops
  // and ships the audio to core to transcribe. State lives in the recorder.
  private async toggleRecording(mic: HTMLButtonElement): Promise<void> {
    if (this.recorder.recording) {
      mic.classList.remove("active");
      mic.textContent = "🎙 Voice";
      setHint("transcribing…");
      let wav: string | null = null;
      try {
        wav = await this.recorder.stop();
      } catch (e) {
        console.warn("[recorder] stop failed", e);
      }
      if (wav) {
        this.conn.send({ type: "audio", wav_b64: wav });
      } else {
        setHint("");
      }
    } else {
      if (!navigator.mediaDevices?.getUserMedia) {
        setHint("mic API unavailable in this webview (no getUserMedia)");
        console.warn("[recorder] navigator.mediaDevices.getUserMedia is undefined");
        return;
      }
      try {
        this.stopSpeech(); // don't capture her own voice back
        await this.recorder.start();
        mic.classList.add("active");
        mic.textContent = "● Recording";
        setState("listening");
        setHint("listening — click again to send");
      } catch (e) {
        // Surface the specific reason so we know if it's a permission denial
        // (NotAllowedError), no device (NotFoundError), etc.
        const name = (e as { name?: string; message?: string })?.name ?? "";
        const msg = (e as { message?: string })?.message ?? String(e);
        console.warn("[recorder] start failed", e);
        setHint(`mic blocked: ${name || msg} — WebView2 denied the microphone`);
      }
    }
  }

  // Activity panel: one row per tool invocation. The first event (STARTED)
  // creates the row with a session-monotonic number; later events (DONE/ERROR)
  // update the same row's pill in place, rather than adding a second row.
  private upsertTool(id: number, name: string, status: string, detail?: string): void {
    const log = document.getElementById("toollog");
    if (!log) return;
    const key = `${this.turnSeq}:${id}`;
    let row = this.toolRows.get(key);
    if (!row) {
      const num = ++this.toolSeq;
      row = document.createElement("div");
      row.className = "tool-line";
      row.dataset.key = key;
      const nameEl = document.createElement("span");
      nameEl.className = "t-name";
      nameEl.textContent = `#${num} ${name}`;
      const pill = document.createElement("span");
      pill.className = `t-pill ${status}`;
      pill.textContent = status;
      row.append(nameEl, pill);
      this.toolRows.set(key, row);
      log.prepend(row);
      // Cap visible rows; forget the keys of any we evict.
      while (log.childElementCount > 10) {
        const last = log.lastElementChild as HTMLElement | null;
        if (!last) break;
        if (last.dataset.key) this.toolRows.delete(last.dataset.key);
        last.remove();
      }
    } else {
      const pill = row.querySelector(".t-pill");
      if (pill) {
        pill.className = `t-pill ${status}`;
        pill.textContent = status;
      }
    }
    if (detail) {
      let d = row.querySelector(".t-detail") as HTMLElement | null;
      if (!d) {
        d = document.createElement("div");
        d.className = "t-detail";
        row.append(d);
      }
      d.textContent = detail;
    }
  }

  private adaptQuality(frameMs: number): void {
    this.frameTimes.push(frameMs);
    if (this.frameTimes.length < 90) return;
    this.frameTimes.sort((a, b) => a - b);
    const p95 = this.frameTimes[Math.floor(this.frameTimes.length * 0.95)];
    this.frameTimes.length = 0;
    // Budget 16.6ms for 60fps; degrade if p95 slips past ~19ms.
    if (p95 > 19 && this.resolutionScale > 0.6) {
      this.resolutionScale = Math.max(0.6, this.resolutionScale - 0.1);
      this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2) * this.resolutionScale);
    } else if (p95 < 12 && this.resolutionScale < 0.9) {
      this.resolutionScale = Math.min(0.9, this.resolutionScale + 0.05);
      this.renderer.setPixelRatio(Math.min(window.devicePixelRatio, 2) * this.resolutionScale);
    }
  }

  start(): void {
    const loop = () => {
      const dt = this.clock.getDelta();
      const elapsed = this.clock.elapsedTime;
      const t0 = performance.now();

      this.drainEvents();
      this.core.update(elapsed, dt);
      this.fxPass.uniforms.uTime.value = elapsed;
      this.composer.render();

      this.adaptQuality(performance.now() - t0);
      requestAnimationFrame(loop);
    };
    requestAnimationFrame(loop);
  }
}

function setText(id: string, text: string): void {
  const el = document.getElementById(id);
  if (el) el.textContent = text;
}

// --- Chat log -------------------------------------------------------------

// A transient one-line hint under the log (voice partials, mic status).
function setHint(text: string): void {
  const el = document.getElementById("livehint");
  if (el) el.textContent = text;
}

function chatScrollToBottom(): void {
  const p = document.getElementById("transcriptPanel");
  if (p) p.scrollTop = p.scrollHeight;
}

// Append a message bubble to the scrollable log and return its element. The log
// is capped so a long session doesn't grow the DOM without bound.
function pushMessage(role: "user" | "pythia", text: string): HTMLElement {
  const log = document.getElementById("chatlog");
  const el = document.createElement("div");
  el.className = `msg ${role}`;
  const who = document.createElement("span");
  who.className = "who";
  who.textContent = role === "user" ? "you" : "pythia";
  const body = document.createElement("span");
  body.className = "body";
  body.textContent = text;
  el.append(who, body);
  if (log) {
    log.appendChild(el);
    while (log.childElementCount > 60) log.firstElementChild?.remove();
  }
  chatScrollToBottom();
  return el;
}

// Stream the growing reply into the live assistant bubble, creating it on the
// first chunk.
function streamReply(current: HTMLElement | null, text: string): HTMLElement {
  const el = current ?? pushMessage("pythia", "");
  const body = el.querySelector(".body");
  if (body) body.textContent = text;
  chatScrollToBottom();
  return el;
}

// Drive the state chip: text + a data-state attribute that colors it (and the
// ambient vignette) via CSS.
function setState(state: string): void {
  const el = document.getElementById("state");
  if (el) {
    el.textContent = state;
    el.dataset.state = state;
  }
  document.body.dataset.state = state;
}


// Bootstrap. When core serves the HUD itself, derive the WebSocket URL from the
// page's own origin so it works on whatever host/port core bound — no hardcoded
// port. An explicit ?ws= overrides; file:// (rare) falls back to the default.
const canvas = document.getElementById("scene") as HTMLCanvasElement | null;
if (canvas) {
  const override = new URLSearchParams(location.search).get("ws");
  const fromOrigin = location.protocol.startsWith("http")
    ? `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/hud`
    : "ws://127.0.0.1:8770/hud";
  const hud = new Hud(canvas, override ?? fromOrigin);
  hud.start();
}

export { Hud };
