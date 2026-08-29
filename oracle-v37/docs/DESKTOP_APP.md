# Oracle of Delphi — the desktop app

This is the native Windows (and Linux) shell for the Oracle. It replaces the
"open a browser tab at the HUD" workflow with a single installable app: a
frameless, glass, always-summonable window that **is** the Oracle, with the
`oracle-core` and `oracle-actd` daemons launched and reaped for you.

Nothing about the assistant itself changed. The desktop shell is a thin host
built on **Tauri v2**: the same Three.js HUD you already had is loaded into a
WebView2 window, and it talks to `oracle-core` over the exact same WebSocket
protocol — same telemetry, same voice loop, same Apollo-decree confirm flow.
All the intelligence, tools, memory, Google, and safety gating still live in the
Rust backend behind the same `oracle.toml`.

## Why Tauri (and not Electron, and not the browser)

- **It's a real .exe, ~10 MB, not a bundled Chromium.** Tauri uses the WebView2
  runtime that ships with Windows 11, so the installer is small and startup is
  instant. Electron would have bolted a second 150 MB browser onto a machine
  that already has one.
- **Web2View keeps the voice loop working.** The HUD's speech in/out uses the
  Web Speech API, which WebView2 supports — so voice survives the move off the
  browser with zero code change.
- **The backend stays exactly as-is.** `oracle-core` and `oracle-actd` are
  shipped as *sidecar* executables. The Rust shell spawns them, and kills them
  when the app quits, so you never leave an orphaned actuator daemon holding the
  named pipe.
- **The window can do what a tab can't:** frameless + transparent so the arc
  core floats on your desktop, a system-tray sun, and a global hotkey
  (`Ctrl+Alt+O`) that summons Pythia from any app.

## Layout

```
oracle-app/
  package.json                 # Tauri CLI + npm scripts (stage / dev / build)
  src-tauri/
    Cargo.toml                 # the "Oracle of Delphi.exe" crate (its own project)
    tauri.conf.json            # window, tray, CSP, bundle + sidecar declarations
    build.rs
    src/main.rs                # spawns sidecars, tray, hotkey, window lifecycle
    capabilities/default.json  # the permissions the shell is allowed to use
    icons/                     # the Apollo-sun app + tray icons (generated)
    binaries/                  # staged oracle-core / oracle-actd (git-ignored)
    stage-sidecars.ps1 / .sh   # build backend + copy binaries here with triple suffix
```

`oracle-app` is intentionally **outside** the Rust workspace (`exclude` in the
root `Cargo.toml`) — it pulls the WebView/GUI system dependencies and is only
ever built on a dev machine, never in the backend's CI.

## Prerequisites (Windows)

1. **Rust** (MSVC toolchain) — you already have this; the backend compiles.
2. **Node.js 18+** — for the Tauri CLI and the HUD build.
3. **WebView2 Runtime** — preinstalled on Windows 11. On Windows 10, grab the
   Evergreen runtime from Microsoft if it's missing.
4. **Microsoft C++ Build Tools** — already present since `cargo build` works.

No AMD/HIP anything is needed for the shell itself; the GPU work all happens in
the llama-server the backend talks to.

## Build it

From `oracle-app/`:

```powershell
npm install                # once: pulls the Tauri CLI
npm run stage              # builds oracle-core + oracle-actd (release) and
                           #   copies them into src-tauri/binaries/ with the
                           #   x86_64-pc-windows-msvc suffix Tauri expects
npm run build              # builds the HUD, compiles the shell, produces the
                           #   installer under src-tauri/target/release/bundle/
```

The installer lands in
`oracle-app/src-tauri/target/release/bundle/nsis/Oracle of Delphi_0.1.0_x64-setup.exe`
(and an `.msi` under `bundle/msi/`).

### Run it live while developing

```powershell
npm run stage              # sidecars must be staged at least once
npm run dev                # hot-reloads the HUD; shell + sidecars run for real
```

`npm run dev` runs the Vite dev server for the HUD and launches the shell
against it, so HUD edits hot-reload while the real `oracle-core`/`oracle-actd`
sidecars run underneath.

## Where it finds your config

The shell launches the sidecars with a `oracle.toml`, resolved in this order:

1. `ORACLE_CONFIG` environment variable, if set;
2. `%APPDATA%\com.oracle-of-delphi.desktop\oracle.toml` (the app config dir —
   this is also where `oracle-core auth` should write your sealed Google token);
3. a `oracle.toml` bundled as an app resource (optional);
4. otherwise, sane defaults (actd pipe `oracle-actd`, no Google, no sensitive
   tier) with a console warning.

The recommended one-time setup mirrors the CLI:

```powershell
# 1) Author the Google token once (same as before)
oracle-core auth --config deploy\oracle.windows.toml `
                 --credentials E:\oracle-models\credentials.json --account default

# 2) Drop your rig profile where the app looks for it
mkdir "$env:APPDATA\com.oracle-of-delphi.desktop"
copy deploy\oracle.windows.toml "$env:APPDATA\com.oracle-of-delphi.desktop\oracle.toml"
```

The shell reads only `[actd].socket` and `[actd].grant_sensitive` from that file
(to launch the daemon in lockstep); everything else is core's business, exactly
as on the CLI.

## Using it

- **Summon / dismiss:** `Ctrl+Alt+O` from anywhere, the tray sun, or clicking
  the tray icon.
- **Move it:** drag the thin titlebar strip (it's a Tauri drag region).
- **Close vs quit:** the window's ✕ *hides* to the tray (the Oracle keeps
  running and listening); a real quit is "Close the Temple" from the tray menu,
  which also reaps both daemons.
- **Everything else** — voice, text, tool calls, the Apollo decree modal for
  irreversible actions — works exactly as in the browser HUD.

## Regenerating the icons

The Apollo-sun icon set is generated, not hand-drawn:

```powershell
python src-tauri\gen_icons.py     # needs Pillow: pip install pillow
```

It writes `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.ico`, and the
tray glyph into `src-tauri/icons/`.

## Note on cross-compiling

The `.exe` is built **on Windows**, by you — a Tauri Windows bundle can't be
produced from the Linux dev sandbox (no WebView2/MSVC target there, and the
toolchain mirror is locked down). The backend crates and the HUD are fully
verified in CI; the shell is the one piece that compiles on your machine. Given
your workspace already builds the whole backend natively in ~44 s, the shell
adds only the Tauri crates on top.
