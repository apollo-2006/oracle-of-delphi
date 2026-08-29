# Windows-Native Setup (PowerShell, no WSL)

This guide covers running Oracle of Delphi natively on Windows, targeting an AMD RX
9070 XT (RDNA4). It complements `docs/DEPLOYMENT.md` (which is Linux-leaning).

## What's Windows-specific in the codebase

| Concern | Linux | Windows (this build) |
|---|---|---|
| core ↔ actd IPC | Unix domain socket | **Named pipe** `\\.\pipe\oracle-actd` (`oracle_ipc::transport::windows`) |
| actd window/process/input | `/proc` + (x11/uinput) | **Win32 PAL**: EnumWindows, Toolhelp32, SendInput (`pal::windows::WindowsPlatform`) |
| audio capture | ALSA | **WASAPI** (`capture_wasapi.cpp`), selects your device by name |
| browser open (auth) | xdg-open | `cmd /C start` |
| vault key | keyring (Secret Service) | keyring (DPAPI) — or the file fallback below |

All of it is `#[cfg(windows)]` / `#ifdef _WIN32` and compiles on your machine
with the MSVC toolchain. (It is not built in the Linux CI, so treat the first
Windows `cargo build` as the compile check for these paths.)

## 1. Toolchain

```powershell
# Rust (MSVC toolchain) + Visual Studio Build Tools (C++), CMake, Node.
rustup default stable-x86_64-pc-windows-msvc
winget install Kitware.CMake OpenJS.NodeJS Microsoft.VisualStudio.2022.BuildTools
```

## 2. Build

```powershell
cargo build --release                          # core + actd (named-pipe + Win32 PAL)
cmake -B oracle-audio\build -S oracle-audio     # WASAPI is auto-enabled on Windows
cmake --build oracle-audio\build --config Release
cd oracle-hud; npm install; npm run build; cd ..
```

Copy `deploy\oracle.windows.toml` to `%APPDATA%\oracle\oracle.toml` and adjust
paths. It's already set for your rig: model dir `E:\oracle-models`, mic
`Microphone (Razer Seiren V3 Mini)`, output `Speakers (3- Fosi Audio K5 Pro)`,
named pipe `oracle-actd`.

Validate it:

```powershell
.\target\release\oracle-core.exe check-config --config $env:APPDATA\oracle\oracle.toml
```

## 3. The LLM on your RX 9070 XT — ROCm vs Vulkan

Short version: **use the Vulkan build of llama.cpp first.** It's the low-friction
path on Windows, needs no ROCm install (just your Adrenalin driver + the Vulkan
runtime you already have), and performs competitively on RDNA4. Oracle of Delphi
only talks to an OpenAI-compatible endpoint, so it doesn't care which backend
llama-server uses.

The current (2026) situation for the 9070 XT (gfx1201) on Windows:

- **ROCm/HIP on Windows does now cover RDNA4** — gfx1200/gfx1201 are among the
  few GPUs supported by the Windows HIP SDK (ROCm 7.x). So a HIP build *can*
  run on your card, and community RDNA4 builds have reported ~99 tok/s.
- **But Vulkan is the pragmatic default.** One documented 9070 XT benchmark put
  llama.cpp-Vulkan at ~62 tok/s; Vulkan needs no SDK and "just works." ROCm can
  edge it out with the right build, at the cost of a heavier setup and a
  known idle-GPU-usage quirk on RDNA4.

**I am NOT bundling HIP/ROCm binaries** — that's AMD's proprietary runtime and
not mine to ship. You install one of these yourself:

Option A — Vulkan (recommended to start):
```powershell
# Grab a prebuilt llama.cpp Vulkan release, or build:
cmake -B build -DGGML_VULKAN=ON
cmake --build build --config Release
.\build\bin\llama-server.exe -m E:\oracle-models\qwen2.5-14b-instruct-q5_k_m.gguf `
  --host 127.0.0.1 --port 8080 --ctx-size 32768
```

Option B — ROCm/HIP (more setup, potentially higher tok/s):
Install the AMD **HIP SDK for Windows** (ROCm 7.x, which lists gfx1201), then
build llama.cpp with `-DGGML_HIP=ON -DAMDGPU_TARGETS=gfx1201`, or use a prebuilt
RDNA4 ROCm release. If ROCm doesn't detect the card, fall back to Vulkan.

Either way, set in your config:
```toml
[llm]
backend = "http://127.0.0.1:8080"
model = "qwen2.5-14b-instruct-q5_k_m"
model_dir = "E:\\oracle-models"
```

Sources: [Vulkan vs ROCm on RDNA4 (2026)](https://runaihome.com/blog/rdna4-vulkan-vs-rocm-local-llm-benchmark-2026/), [ROCm 7.2 on Windows, RDNA 3 & 4](https://runaihome.com/blog/amd-rocm-local-ai-2026/), [llama.cpp RDNA4 gfx1201 build](https://github.com/tlee933/llama.cpp-rdna4-gfx1201), [llamacpp-rocm prebuilts](https://github.com/lemonade-sdk/llamacpp-rocm).

## 4. Google Workspace auth

Put your `credentials.json` somewhere private (NOT in the repo). Then:

```powershell
.\target\release\oracle-core.exe auth `
  --config $env:APPDATA\oracle\oracle.toml `
  --credentials C:\path\to\credentials.json `
  --account you@gmail.com
```

It opens your browser, you consent, and the loopback server captures the code
and seals the refresh token. Your Desktop-app client's redirect is
`http://localhost`; Google's loopback flow accepts the `127.0.0.1:<port>` the
app listens on. The client_secret from `credentials.json` is sent in the token
exchange (required for Google Desktop apps) and is never logged.

### OAuth troubleshooting

- **"Missing required parameter: redirect_uri" (Error 400)** — fixed. This was
  the Windows browser launcher routing the URL through `cmd /C start`, whose
  parser splits on the `&` between query params and truncated the URL. The
  launcher now uses `rundll32 url.dll,FileProtocolHandler`, which passes the URL
  verbatim. If you're on an older build, copy the URL that's printed to the
  console into your browser manually — that always works.
- **"redirect_uri_mismatch"** — if you hit this after the above fix, Google
  isn't accepting the loopback path. It's a one-line change in
  `LoopbackServer::redirect_uri()`: drop the `/callback` path (use
  `http://127.0.0.1:{port}`) or switch the host to `localhost`. The loopback
  server parses the code regardless of path, so either works. Tell me and I'll
  patch it.
- **"access_blocked / app not verified"** — while your OAuth consent screen is
  in "Testing", add your Google account under *OAuth consent screen → Test
  users*. Unverified test apps work fine for your own account.

> Security note: keep `credentials.json` and the generated `vault.key` /
> `google-*.tok` out of source control and backups you don't trust. The file
> `vault.key` is the fallback key store; for hardening, switch the vault to
> DPAPI-backed keyring storage (one trait swap — see `connectors::vault`).

## 5. Run

```powershell
# actd first (privileged actuator, owns the named pipe).
# Add --grant-sensitive to allow input injection (os.type_text). Irreversible
# ops (kill process, risky shell) STILL require confirmation regardless.
Start-Process .\target\release\oracle-actd.exe -ArgumentList '--serve','oracle-actd','--grant-sensitive'

# then core (loads Google + connects to actd automatically)
.\target\release\oracle-core.exe run --config $env:APPDATA\oracle\oracle.toml

# audio engine (WASAPI capture from the Razer mic) — optional; the HUD's voice
# button gives you speech-in/out without it (see below).
.\oracle-audio\build\Release\oracle-audio.exe
```

## 6. Talking to it: OS control + voice

**OS control.** With actd running, the agent gets `os.list_windows`,
`os.list_processes`, `os.focus_window`, `os.shell`, `os.type_text`, and
`os.kill_process`. Ask it things like "what windows do I have open?" or "run
`dir` and tell me the biggest file". Read-only shell runs directly; risky
commands and process-kills are refused pending confirmation (safe by design).

**Voice (two paths):**
- **Browser voice (works today, zero setup).** Open the HUD, click **🎙 Voice**.
  It uses Chrome's built-in speech recognition to send what you say as a
  message, and speaks the reply back. Talking over a reply barges in (cancels
  the speech and interrupts the turn). This needs a Chromium-based browser.
- **Native audio engine (offline/production).** `oracle-audio.exe` (WASAPI
  capture from the Razer mic) is the local path; it still needs a local ASR
  (whisper.cpp) and a neural TTS wired in — that integration is the remaining
  work. The browser path is the one that works right now.

The core prints the HUD URL with a token; open it in a browser.

Run them as background services with NSSM or a Scheduled Task if you want them
to survive logout. (The systemd units in `deploy/systemd/` are Linux-only.)

## 6. Known Windows caveats

- **First Windows compile is the real check** for the named-pipe transport, the
  Win32 PAL, and WASAPI — those paths aren't built in the Linux CI. If anything
  in `windows-sys` or the Core Audio COM calls doesn't line up with your SDK
  version, it'll surface here; the logic is straightforward to fix in place.
- **Foreground-window rules:** `SetForegroundWindow` can be refused by Windows'
  foreground lock unless the caller has recent input focus. That's a Windows
  policy, not a bug; the daemon reports it honestly.
- **RT audio:** Windows uses MMCSS "Pro Audio" for the capture thread rather
  than SCHED_FIFO; WASAPI event-driven mode already gives tight periods.
