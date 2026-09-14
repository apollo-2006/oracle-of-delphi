// Entry point for the static demo build (`npm run build:demo`). Installs the
// scripted gateway in place of WebSocket, adds a banner that says so, then loads
// the real HUD, which is not changed in any way.
import { ScriptedGateway } from "./scriptedGateway.js";

(window as unknown as { WebSocket: unknown }).WebSocket = ScriptedGateway;

// Voice input needs the local speech-to-text process. Without this, the HUD sees
// the browser's SpeechRecognition, arms its wake-word listener at startup, and a
// visitor can get a microphone prompt before touching anything. Spoken replies
// use speechSynthesis and keep working.
for (const name of ["SpeechRecognition", "webkitSpeechRecognition"]) {
  Object.defineProperty(window, name, { value: undefined, configurable: true, writable: true });
}

const banner = document.createElement("div");
banner.id = "demoBanner";
banner.innerHTML = `
  <strong>Live HUD, scripted core.</strong>
  This is the real Oracle of Delphi interface. The assistant behind it runs locally on a GPU, so here its replies are
  scripted. Try: <em>close chrome</em> · <em>what's using my memory</em> · <em>remember my exam is friday</em> · <em>open spotify</em>.
  <span class="links">
    <a href="https://github.com/apollo-2006/oracle-of-delphi">source</a> ·
    <a href="https://abirdeol.tech/projects/oracle-of-delphi">write-up</a> ·
    <a href="https://abirdeol.tech/projects?filter=live">more demos</a>
  </span>
  <button type="button" aria-label="Dismiss">✕</button>`;
const style = document.createElement("style");
style.textContent = `
  #demoBanner { position: fixed; left: 50%; top: 52px; transform: translateX(-50%); z-index: 50;
    width: max-content; max-width: min(760px, calc(100vw - 32px)); padding: 10px 40px 10px 14px; border-radius: 12px;
    background: rgba(9, 15, 26, 0.86); border: 1px solid rgba(140, 200, 255, 0.22); color: #cfe0f0;
    font: 12.5px/1.5 "Segoe UI", system-ui, sans-serif; backdrop-filter: blur(10px); }
  #demoBanner strong { color: #f5c451; font-weight: 600; }
  #demoBanner em { color: #35c9ff; font-style: normal; }
  #demoBanner a { color: #8aa0b8; }
  #demoBanner .links { white-space: nowrap; }
  #demoBanner button { position: absolute; top: 6px; right: 8px; background: none; border: 0; color: #8aa0b8; cursor: pointer; font-size: 14px; }
  @media (max-width: 600px) { #demoBanner { font-size: 11.5px; padding: 8px 32px 8px 12px; top: 48px; } }
  /* Window controls for the native shell, which a browser tab does not have. */
  #expand, #retract, #wake { display: none !important; }
`;
document.head.append(style);
document.body.append(banner);
banner.querySelector("button")?.addEventListener("click", () => banner.remove());

await import("../main.js");

const mic = document.getElementById("mic") as HTMLButtonElement | null;
if (mic) {
  mic.disabled = true;
  mic.title = "Voice input needs the local speech-to-text process; type instead";
}
