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
  private pendingReply = "";
  private apollo: ApolloModal;
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
        this.conn.send({ type: "user_text", text });
        setText("transcript", text);
        setText("caption", "");
      },
      onPartial: (text) => setText("transcript", text + " …"),
      onBargeIn: () => this.conn.send({ type: "interrupt" }),
      onStatus: (msg) => setText("transcript", "🎙 " + msg),
      onWake: (command) => {
        // Wake word heard — raise the window (core relays to the shell) and
        // show we're attending. A bare "Delphi" leaves the mic open briefly for
        // the follow-up; "Delphi, do X" already forwarded X as the utterance.
        this.conn.send({ type: "summon" });
        setState("listening");
        setText("transcript", "");
        setText("caption", command ? "" : "Yes? I'm listening…");
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
    interrupt?.addEventListener("click", () => this.conn.send({ type: "interrupt" }));

    const input = document.getElementById("userInput") as HTMLInputElement | null;
    input?.addEventListener("keydown", (e: KeyboardEvent) => {
      if (e.key === "Enter" && input.value.trim().length > 0) {
        const text = input.value.trim();
        this.conn.send({ type: "user_text", text });
        // Echo locally so the user sees their message immediately.
        setText("transcript", text);
        setText("caption", "");
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
        const on = this.voice.toggle();
        mic.classList.toggle("active", on);
        mic.textContent = on ? "🎙 Listening…" : "🎙 Voice";
      });
    }

    // Wake-word listener ("Delphi"). Armed by default when voice is supported so
    // the Oracle answers to its name hands-free; the chip lets the user silence
    // it. Browsers may block starting the mic without a gesture, so we also arm
    // on the first interaction as a fallback.
    const wake = document.getElementById("wake") as HTMLButtonElement | null;
    if (wake) {
      if (!this.voice.supported) {
        wake.disabled = true;
        wake.hidden = true;
      } else {
        const reflect = (on: boolean) => {
          wake.classList.toggle("on", on);
          wake.textContent = on ? "◉ Delphi" : "○ Wake off";
          wake.title = on
            ? 'Listening for "Delphi" — click to disable'
            : 'Say "Delphi" to summon — click to enable';
        };
        reflect(this.voice.enableWake(true));
        wake.addEventListener("click", () => reflect(this.voice.enableWake(!this.voice.isWakeOn)));
        // Fallback arm: if autostart was blocked (no user gesture yet), the
        // first click/keypress re-arms it.
        const rearm = () => {
          if (this.voice.isWakeOn) reflect(this.voice.enableWake(true));
          window.removeEventListener("pointerdown", rearm);
          window.removeEventListener("keydown", rearm);
        };
        window.addEventListener("pointerdown", rearm);
        window.addEventListener("keydown", rearm);
      }
    }

    // Spoken-reply toggle. On by default; lets the user silence TTS.
    const speak = document.getElementById("speak") as HTMLButtonElement | null;
    if (speak) {
      speak.classList.toggle("active", this.voiceOut);
      speak.addEventListener("click", () => {
        this.voiceOut = !this.voiceOut;
        speak.classList.toggle("active", this.voiceOut);
        speak.textContent = this.voiceOut ? "🔊 Voice reply" : "🔇 Muted";
        if (!this.voiceOut) this.voice.cancelSpeak();
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
        // When a turn returns to idle, speak the accumulated reply (if voice-out
        // is on).
        if (ev.state === "idle" && this.pendingReply.trim().length > 0) {
          if (this.voiceOut) this.voice.speak(this.pendingReply);
          this.pendingReply = "";
        }
        break;
      case "transcript":
        setText("transcript", ev.text + (ev.stable ? "" : " …"));
        break;
      case "caption":
        // Caption streams the growing reply; keep the latest for TTS.
        this.pendingReply = ev.text;
        setText("caption", ev.text);
        break;
      case "tool":
        appendToolLog(ev.id, ev.name, ev.status, ev.detail);
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

// A styled activity row: `#id name`, a colored status pill, and (on failure)
// the error detail on its own line.
function appendToolLog(id: number, name: string, status: string, detail?: string): void {
  const el = document.getElementById("toollog");
  if (!el) return;
  const row = document.createElement("div");
  row.className = "tool-line";

  const nameEl = document.createElement("span");
  nameEl.className = "t-name";
  nameEl.textContent = `#${id} ${name}`;

  const pill = document.createElement("span");
  pill.className = `t-pill ${status}`;
  pill.textContent = status;

  row.append(nameEl, pill);
  if (detail) {
    const d = document.createElement("div");
    d.className = "t-detail";
    d.textContent = detail;
    row.append(d);
  }
  el.prepend(row);
  while (el.childElementCount > 10) el.lastElementChild?.remove();
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
