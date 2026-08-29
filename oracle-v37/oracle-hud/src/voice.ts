// Browser voice loop (architecture §1, pragmatic path).
//
// Uses the browser's built-in Web Speech API — SpeechRecognition for ASR and
// SpeechSynthesis for TTS — so voice works TODAY on the user's Chrome with no
// local whisper/piper models. It wires into the SAME plumbing as the text box:
// recognized speech is sent as `user_text`; assistant captions are spoken.
//
// Barge-in: when the user starts talking while Oracle of Delphi is speaking, we
// cancel synthesis and send `interrupt`, mirroring the native audio engine's
// behavior. The local C++ audio engine (WASAPI + whisper + a neural vocoder)
// remains the offline/production path; this is the zero-setup one.

// Minimal typings for the vendor-prefixed API (not in lib.dom for all TS libs).
interface SpeechRecognitionResultLike {
  0: { transcript: string };
  isFinal: boolean;
}
interface SpeechRecognitionEventLike {
  resultIndex: number;
  results: { length: number; [i: number]: SpeechRecognitionResultLike };
}
interface SpeechRecognitionLike {
  continuous: boolean;
  interimResults: boolean;
  lang: string;
  start(): void;
  stop(): void;
  onresult: ((e: SpeechRecognitionEventLike) => void) | null;
  onerror: ((e: unknown) => void) | null;
  onend: (() => void) | null;
  onstart: (() => void) | null;
  onaudiostart: (() => void) | null;
  onspeechstart: (() => void) | null;
}
type SpeechRecognitionCtor = new () => SpeechRecognitionLike;

function getRecognitionCtor(): SpeechRecognitionCtor | null {
  const w = window as unknown as {
    SpeechRecognition?: SpeechRecognitionCtor;
    webkitSpeechRecognition?: SpeechRecognitionCtor;
  };
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

export interface VoiceCallbacks {
  /** Final recognized utterance → send as user_text. */
  onUtterance: (text: string) => void;
  /** User started speaking → barge-in (cancel TTS + interrupt). */
  onBargeIn: () => void;
  /** Interim/partial transcript for the HUD. */
  onPartial?: (text: string) => void;
  /** Lifecycle/diagnostic messages (listening, errors) for the HUD + console. */
  onStatus?: (msg: string) => void;
  /**
   * The wake word ("Delphi") was heard. `command` is any speech that followed
   * it in the same breath ("Delphi, what's my schedule" → "what's my
   * schedule"), or "" for a bare summon. Fired even while dismissed, so the
   * HUD can raise the window and then listen for / act on the command.
   */
  onWake?: (command: string) => void;
}

// Matches "Delphi" and the ways browser ASR commonly renders it (delphie,
// delfi, delphy, "dell fee"…). The "del" + f/ph + vowel skeleton keeps everyday
// words ("delta", "delve", "deli") from tripping it.
const WAKE_RE = /\bdel\s?(ph|f)[iy]e?\b/i;

/** Split an utterance on the wake word: does it contain it, and what follows? */
export function matchWake(text: string): { hit: boolean; command: string } {
  const m = WAKE_RE.exec(text);
  if (!m) return { hit: false, command: "" };
  // Everything after the wake word is the command; strip leading punctuation.
  const tail = text.slice(m.index + m[0].length).replace(/^[\s,.;:!?—-]+/, "");
  return { hit: true, command: tail.trim() };
}

/**
 * Manages speech recognition (mic) + synthesis (speaker).
 *
 * Two listening intents share ONE recognition stream (a second stream would
 * fight for the mic): `wantWake` keeps it always-on scanning for the wake word,
 * while `active` (the mic button, or a wake-triggered window) forwards every
 * utterance as a command. Recognition runs whenever either is set.
 */
export class VoiceLoop {
  private recog: SpeechRecognitionLike | null = null;
  private running = false;
  private speaking = false;
  private wantWake = false;
  private active = false;
  private activeTimer: ReturnType<typeof setTimeout> | null = null;
  private micWatchdog: ReturnType<typeof setTimeout> | null = null;
  private micOpened = false;
  private cb: VoiceCallbacks;

  constructor(cb: VoiceCallbacks) {
    this.cb = cb;
  }

  get supported(): boolean {
    return getRecognitionCtor() !== null && "speechSynthesis" in window;
  }

  /** Whether recognized speech is currently being forwarded as commands. */
  get isListening(): boolean {
    return this.active;
  }

  /** Whether the always-on wake-word listener is armed. */
  get isWakeOn(): boolean {
    return this.wantWake;
  }

  /** Toggle active (forward-everything) listening — the mic button. */
  toggle(): boolean {
    if (this.active) {
      this.setActive(false);
    } else {
      this.setActive(true);
    }
    return this.active;
  }

  /** Arm or disarm the always-on wake-word listener. Returns the new state. */
  enableWake(on: boolean): boolean {
    this.wantWake = on;
    if (on) {
      this.ensureRunning();
    } else {
      this.maybeStop();
    }
    return this.wantWake;
  }

  /** Enter/leave active listening, keeping the shared stream in the right mode. */
  private setActive(on: boolean, autoRevertMs?: number): void {
    if (this.activeTimer) {
      clearTimeout(this.activeTimer);
      this.activeTimer = null;
    }
    this.active = on;
    if (on) {
      this.ensureRunning();
      this.cb.onStatus?.("listening — speak now");
      if (autoRevertMs) {
        // A bare wake ("Delphi") opens a short command window, then falls back
        // to wake-only so we're not forwarding ambient chatter forever.
        this.activeTimer = setTimeout(() => this.setActive(false), autoRevertMs);
      }
    } else {
      this.maybeStop();
    }
  }

  /** Start the recognition stream if it isn't already running. */
  private ensureRunning(): void {
    if (this.running) return;
    const Ctor = getRecognitionCtor();
    if (!Ctor) return;
    const r = new Ctor();
    r.continuous = true;
    r.interimResults = true;
    r.lang = "en-US";
    r.onstart = () => {
      console.log("[voice] recognition started", { wake: this.wantWake, active: this.active });
      // Watchdog: if the mic never actually opens (onaudiostart) shortly after
      // start, the OS/WebView denied it — tell the user something actionable
      // instead of leaving them wondering why nothing happens.
      this.micOpened = false;
      if (this.micWatchdog) clearTimeout(this.micWatchdog);
      this.micWatchdog = setTimeout(() => {
        if (!this.micOpened) {
          this.cb.onStatus?.(
            "mic isn't opening — check Windows Settings ▸ Privacy ▸ Microphone (allow desktop apps)",
          );
        }
      }, 4500);
    };
    r.onaudiostart = () => {
      console.log("[voice] audio capture started (mic is live)");
      this.micOpened = true;
      if (this.micWatchdog) {
        clearTimeout(this.micWatchdog);
        this.micWatchdog = null;
      }
    };
    r.onspeechstart = () => {
      console.log("[voice] speech detected");
      // Barge-in: if we're mid-utterance, cut it off.
      if (this.speaking) {
        this.cancelSpeak();
        this.cb.onBargeIn();
      }
    };
    r.onresult = (e: SpeechRecognitionEventLike) => {
      for (let i = e.resultIndex; i < e.results.length; i++) {
        const res = e.results[i];
        const text = res[0].transcript.trim();
        console.log("[voice] result", { final: res.isFinal, text, active: this.active });
        if (res.isFinal) {
          if (text.length > 0) this.handleFinal(text);
        } else if (this.active) {
          // Only paint partials while actively listening — the wake stream
          // shouldn't splash ambient speech across the transcript.
          this.cb.onPartial?.(text);
        }
      }
    };
    r.onend = () => {
      console.log("[voice] recognition ended", { running: this.running });
      // Auto-restart while either intent still wants the mic (continuous mode
      // stops on silence in some browsers).
      if (this.running) {
        try {
          r.start();
        } catch {
          /* already started */
        }
      }
    };
    r.onerror = (e: unknown) => {
      const err = (e as { error?: string }).error ?? "unknown";
      console.warn("[voice] error:", err, e);
      // "no-speech" and "aborted" are benign (silence / restart); surface the
      // ones that mean it's actually broken so the user isn't left guessing.
      if (err !== "no-speech" && err !== "aborted") {
        this.cb.onStatus?.(`voice error: ${err}`);
      }
      if (err === "not-allowed" || err === "service-not-allowed") {
        // Permission was denied — stop the current stream but KEEP the wake
        // intent, so a later user gesture (or the shell granting mic access on
        // the next launch) can re-arm it instead of it being dead for the
        // session. Surface a clear, actionable message.
        this.running = false;
        this.active = false;
        if (this.micWatchdog) {
          clearTimeout(this.micWatchdog);
          this.micWatchdog = null;
        }
        this.cb.onStatus?.(
          "mic blocked — allow microphone for this app in Windows Settings, then click 🎙 Voice",
        );
      }
    };
    this.recog = r;
    this.running = true;
    try {
      r.start();
    } catch (e) {
      console.warn("[voice] start threw", e);
      this.cb.onStatus?.("voice could not start");
    }
  }

  /** Route a final utterance by mode: command when active, wake-scan otherwise. */
  private handleFinal(text: string): void {
    if (this.active) {
      this.cb.onUtterance(text);
      return;
    }
    if (!this.wantWake) return;
    const { hit, command } = matchWake(text);
    if (!hit) return;
    console.log("[voice] wake word", { command });
    this.cb.onWake?.(command);
    if (command.length > 0) {
      // "Pythia, do X" — a complete request in one breath. Send it, stay in
      // wake-only mode (don't leave the mic hot on the room afterward).
      this.cb.onUtterance(command);
    } else {
      // Bare "Delphi" — open a brief command window for the next sentence.
      this.setActive(true, 9000);
    }
  }

  /** Stop the shared stream only if neither intent needs it any more. */
  private maybeStop(): void {
    if (this.wantWake || this.active) return;
    this.running = false;
    if (this.micWatchdog) {
      clearTimeout(this.micWatchdog);
      this.micWatchdog = null;
    }
    this.recog?.stop();
    this.recog = null;
  }

  /** Stop everything — used on teardown. */
  stop(): void {
    this.wantWake = false;
    this.active = false;
    if (this.activeTimer) {
      clearTimeout(this.activeTimer);
      this.activeTimer = null;
    }
    if (this.micWatchdog) {
      clearTimeout(this.micWatchdog);
      this.micWatchdog = null;
    }
    this.running = false;
    this.recog?.stop();
    this.recog = null;
  }

  /** Speak text via the browser's TTS. Marks speaking state for barge-in. */
  speak(text: string): void {
    if (!("speechSynthesis" in window) || text.trim().length === 0) return;
    this.cancelSpeak();
    const u = new SpeechSynthesisUtterance(text);
    u.rate = 1.05;
    u.pitch = 1.0;
    u.onstart = () => {
      this.speaking = true;
    };
    u.onend = () => {
      this.speaking = false;
    };
    window.speechSynthesis.speak(u);
  }

  cancelSpeak(): void {
    if ("speechSynthesis" in window) {
      window.speechSynthesis.cancel();
    }
    this.speaking = false;
  }
}
