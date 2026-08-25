# Oracle of Delphi — native window (oracle-shell)

This is the real "it's an app, not a browser tab" answer: a tiny native window
built directly on **tao + wry** (the windowing + WebView2 libraries) — **not the
Tauri framework**, so there's no bundler, no `tauri.conf.json`, no capabilities
files, no sidecar manifests. Just a window.

It does three things:

- **Is the window.** A real native window (own icon, own taskbar entry) showing
  the HUD that `oracle-core` already serves at `http://127.0.0.1:8770`. No tabs,
  no address bar, no browser.
- **Summons on your terms.** A global hotkey — **Ctrl+Alt+O** — toggles it from
  anywhere. Closing the window hides it to the tray; it's never gone, just
  dismissed. A tray sun re-summons it too.
- **Owns the lifecycle.** On launch it starts `oracle-core` (hidden, with
  `--no-window`), which in turn brings up llama-server and actd. "Quit Oracle"
  from the tray shuts the whole thing down.

Everything else — the LLM, tools, voice, memory, the Apollo confirm flow — stays
in `oracle-core`. The shell is deliberately dumb.

## Build it

From `oracle-shell\`:

```powershell
.\build-app.ps1
```

That builds the backend + the shell and drops `oracle-core.exe` and
`oracle-actd.exe` next to `oracle-of-delphi.exe`, so the whole app is one folder.
Requires the WebView2 runtime (preinstalled on Windows 11) and the Rust MSVC
toolchain you already have.

## Run it

Double-click **`oracle-shell\target\release\oracle-of-delphi.exe`** — or make a
desktop shortcut to it, or drop that shortcut in `shell:startup` to have the
Oracle available at login. First launch waits ~15s while the 14B model loads,
then Pythia answers.

- **Summon / dismiss:** `Ctrl+Alt+O` from anywhere, or click the tray sun.
- **Close button:** hides to the tray (the Oracle keeps running).
- **Quit fully:** "Quit Oracle" from the tray menu.

## How it finds things

- **Backend binary:** `ORACLE_CORE_EXE` if set, else `oracle-core.exe` sitting
  next to the shell (which `build-app.ps1` arranges).
- **Config:** `ORACLE_CONFIG` if set, else `%APPDATA%\oracle\oracle.toml` — the
  same config the CLI uses (with your `[supervise]` llama settings).

## Not done yet

The **"Pythia" wake-word** (summon by voice while the window is dismissed) is the
next step. It needs an always-listening local detector in the audio path; the
hotkey is the reliable summon for now, and the window's own voice button handles
speech once it's open.
