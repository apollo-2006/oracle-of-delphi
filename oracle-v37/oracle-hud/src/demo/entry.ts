// Entry point for the static demo build (`npm run build:demo`). Installs the
// scripted gateway in place of WebSocket, adds a banner that says so, then loads
// the real HUD, which is not changed in any way.
import { ScriptedGateway } from "./scriptedGateway.js";

(window as unknown as { WebSocket: unknown }).WebSocket = ScriptedGateway;

const banner = document.createElement("div");
banner.id = "demoBanner";
banner.innerHTML = `
  <strong>Live HUD, scripted core.</strong>
  This is the real Oracle of Delphi interface. The assistant behind it runs locally on a GPU, so here its replies are
  scripted. Try: <em>close chrome</em> · <em>what's using my memory</em> · <em>remember my exam is friday</em> · <em>open spotify</em>.
  <a href="https://github.com/apollo-2006/oracle-of-delphi">source</a> ·
  <a href="https://abirdeol.tech/projects/oracle-of-delphi">write-up</a>
  <button type="button" aria-label="Dismiss">✕</button>`;
const style = document.createElement("style");
style.textContent = `
  #demoBanner { position: fixed; left: 50%; top: 52px; transform: translateX(-50%); z-index: 50;
    max-width: min(760px, calc(100vw - 32px)); padding: 10px 40px 10px 14px; border-radius: 12px;
    background: rgba(9, 15, 26, 0.86); border: 1px solid rgba(140, 200, 255, 0.22); color: #cfe0f0;
    font: 12.5px/1.5 "Segoe UI", system-ui, sans-serif; backdrop-filter: blur(10px); }
  #demoBanner strong { color: #f5c451; font-weight: 600; }
  #demoBanner em { color: #35c9ff; font-style: normal; }
  #demoBanner a { color: #8aa0b8; }
  #demoBanner button { position: absolute; top: 6px; right: 8px; background: none; border: 0; color: #8aa0b8; cursor: pointer; font-size: 14px; }
`;
document.head.append(style);
document.body.append(banner);
banner.querySelector("button")?.addEventListener("click", () => banner.remove());

await import("../main.js");
