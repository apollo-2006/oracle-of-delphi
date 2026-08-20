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
}

/** Manages speech recognition (mic) + synthesis (speaker). */
export class VoiceLoop {
  private recog: SpeechRecognitionLike | null = null;
  private listening = false;
  private speaking = false;
  private cb: VoiceCallbacks;

  constructor(cb: VoiceCallbacks) {
    this.cb = cb;
  }

  get supported(): boolean {
    return getRecognitionCtor() !== null && "speechSynthesis" in window;
  }

  /** Toggle continuous listening. Returns the new state. */
  toggle(): boolean {
    if (this.listening) {
      this.stop();
    } else {
      this.start();
    }
    return this.listening;
  }

  start(): void {
    const Ctor = getRecognitionCtor();
    if (!Ctor) return;
    const r = new Ctor();
    r.continuous = true;
    r.interimResults = true;
    r.lang = "en-US";
    r.onstart = () => {
      console.log("[voice] recognition started");
      this.cb.onStatus?.("listening — speak now");
    };
    r.onaudiostart = () => {
      console.log("[voice] audio capture started (mic is live)");
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
        console.log("[voice] result", { final: res.isFinal, text });
        if (res.isFinal) {
          if (text.length > 0) this.cb.onUtterance(text);
        } else {
          this.cb.onPartial?.(text);
        }
      }
    };
    r.onend = () => {
      console.log("[voice] recognition ended", { stillListening: this.listening });
      // Auto-restart while the user has listening enabled (continuous mode
      // stops on silence in some browsers).
      if (this.listening) {
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
        // Permission was denied — stop trying so we don't loop.
        this.listening = false;
      }
    };
    this.recog = r;
    this.listening = true;
    try {
      r.start();
    } catch (e) {
      console.warn("[voice] start threw", e);
      this.cb.onStatus?.("voice could not start");
    }
  }

  stop(): void {
    this.listening = false;
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

  get isListening(): boolean {
    return this.listening;
  }
}
