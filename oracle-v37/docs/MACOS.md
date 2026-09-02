# Running on macOS (Apple Silicon)

macOS is a first-class target alongside Linux and Windows. The Rust workspace,
the C++ audio engine, the HUD and the native shell all build and test clean on
Apple Silicon. What follows is only the parts that genuinely differ from the
other two targets: the permissions the actuator needs, the inference backend,
the length limit on the actd socket, and the fact that the voice binaries
vendored in this repo are Windows-only and must be rebuilt.

## 1. Grant the two TCC permissions

The actuator drives the Accessibility API. macOS gates that behind two separate
grants, and **neither can be requested programmatically** — you have to turn them
on by hand, once.

System Settings → Privacy & Security →

* **Accessibility** — required for window control, input injection, and reading
  the UI tree. Without it every one of those operations fails.
* **Automation** — the first Apple event raises a consent prompt. If you deny it,
  the denial is remembered; reset with `tccutil reset AppleEvents`.

**Grant them to whichever binary is actually running.** In a dev run that is your
terminal (Terminal.app, iTerm, or your IDE). In a bundled build it is Oracle.app.
Granting the wrong one is the single most common reason actuation silently fails.

A missing grant surfaces as a named error telling you which setting to enable —
not a silent no-op:

```
injection blocked: macOS denied Accessibility access, which is required for
window control and input injection. Enable it under System Settings → Privacy &
Security → Accessibility.
```

Operations needing **no** permission: `list_processes`, `open_target`,
`kill_process`, `lock_screen`, and volume control.

## 2. Build llama.cpp with Metal

The ROCm/HIP path in `docs/DEPLOYMENT.md` is AMD-specific and does not exist on
macOS. Apple Silicon uses the Metal backend, which is on by default:

```bash
cmake -B build -DGGML_METAL=ON
cmake --build build --config Release -j
```

Note the output path: `build/bin/llama-server`, with **no `Release/` component
and no `.exe`**. The Windows profile's `build\bin\Release\llama-server.exe` does
not exist here, and pointing at it shows up only as a supervised child that
restart-loops in `llm.log`.

`-ngl 99` offloads every layer to the GPU. Unified memory means a 14B model at Q4
wants roughly 10-12 GB: comfortable on a 32 GB machine, and tight on 16 GB, where
it leaves nothing for the OS, the vision tier, or your actual work. **Ship an 8B
on 16 GB.**

You do not have to write the config by hand — `deploy/oracle.macos.toml` is the
macOS counterpart of the Windows profiles, with the paths, the Metal flags and
the 8B planner already set:

```bash
mkdir -p ~/Library/Application\ Support/oracle
cp deploy/oracle.macos.toml ~/Library/Application\ Support/oracle/oracle.toml
# then check it, before a long build rather than after:
cargo run -p oracle-core -- check-config \
  --config ~/Library/Application\ Support/oracle/oracle.toml
```

Edit the absolute paths at the top if your checkout is not at
`~/projects/oracle-of-delphi` — TOML does not expand `~` or `$HOME`.

### Keep the actd socket path short

This is the one macOS trap that fails at *run* time rather than at load, and it
has nothing to do with permissions.

The actd link is a unix socket, and the kernel copies its path into
`sockaddr_un.sun_path` — a fixed array of **104 bytes on macOS and the BSDs**,
108 on Linux. Over the limit, `bind` and `connect` fail with
`InvalidInput: path must be shorter than SUN_LEN`, an error naming neither the
path nor the limit.

The trap is that the obvious per-user location, `$TMPDIR`, is **not** `/tmp` on
macOS. It is a hashed per-user path like
`/var/folders/jj/cvft_wmn3cs4cl2pywmqvb3w0000gn/T/` — 49 bytes spent before your
config has said anything. A socket path that is comfortably legal on Linux can
therefore be illegal on a Mac.

Config validation now refuses an over-long `[actd] socket` at load, with an error
that states the length and the limit. The shipped profile uses
`/tmp/oracle/actd.sock`, which always fits; actd creates the parent directory,
chmods the socket `0600`, and checks the peer uid on every connection, so `/tmp`
is not a weakening here.

## 3. Grant the microphone

Capture uses AUHAL, bound to the system default input device at its native rate;
the decimator downstream resamples to 16k.

macOS gates the mic behind TCC separately from Accessibility. A bundled app needs
`NSMicrophoneUsageDescription` in its `Info.plist` and the user must approve the
prompt; a plain CLI run inherits the terminal's grant.

If it is missing, **CoreAudio does not fail** — the unit starts cleanly and
delivers silence forever. The backend therefore reports the device it bound on
start, and on stop says so explicitly if nothing was ever delivered:

```
[coreaudio] capturing: 1 ch @ 48000 Hz
[coreaudio] no audio was ever delivered. If this is unexpected, check
Privacy & Security -> Microphone for this binary.
```

## 4. Run the setup script

One command fetches or builds everything that is platform-specific:

```bash
scripts/setup.sh              # piper, whisper.cpp, the whisper model, llama.cpp
scripts/setup.sh whisper      # or just one component
scripts/setup.sh --force      # re-fetch something you suspect is broken
```

It installs into the layout the shipped profiles expect, so nothing needs
editing afterwards:

| Path | What | How |
|---|---|---|
| `.venv/bin/piper` | Text to speech | `piper-tts` wheel |
| `whisper/macos-arm64/` | Speech in + wake word | **built from source** |
| `whisper/models/ggml-base.en.bin` | The ASR model, ~148 MB | downloaded |
| `llama.cpp/build/bin/llama-server` | Inference | built with Metal |

Re-running is safe: each step is skipped when its output is already there.

### Why piper comes from a wheel and not the release

**The published macOS archive is broken.** `piper_macos_aarch64.tar.gz` from
release `2023.11.14-2` — the newest that exists — ships the `piper` and
`piper_phonemize` executables but omits the `.dylib` files they link against,
keeping only an onnxruntime `.dSYM` debug bundle. Extract it and run it and you
get:

```
dyld: Library not loaded: @rpath/libespeak-ng.1.dylib
```

The `piper-tts` **wheel** is the maintained path, publishes a
`macosx_11_0_arm64` build, and accepts `--model` / `--output_file` — the exact
arguments this project already passed to the old CLI, so `[voice] tts_args`
needed no change. It also bundles its own phonemization, so nothing separate has
to be fetched. There is a `win_amd64` wheel too, which is what lets one
mechanism serve both platforms.

The voice model, `piper/en_US-amy-medium.onnx`, is platform-neutral and stays
committed — it is the one vendored file that works everywhere.

### Why whisper.cpp is built rather than downloaded

whisper.cpp publishes release binaries for **Windows and Ubuntu only**; there is
no macOS asset. The script therefore clones and builds it, with SDL2 because
`whisper-stream` opens the microphone itself rather than going through the HUD:

```bash
brew install sdl2      # before scripts/setup.sh whisper
```

Without SDL2, `whisper-cli` still builds and transcription works; only the wake
word is missing. The script says so rather than failing silently.

Leave `-nt` out of `wake_args`. With timestamps on, `whisper-stream` prints one
newline-terminated line per utterance, which core parses; `-nt` makes it redraw
a single line with carriage returns that never split into lines, so the wake
word is never heard.

### The vendored Windows binaries

`piper/*.exe`, `piper/*.dll` and `whisper/*.exe` at the repository root are
Windows builds and do not run here. They are still committed so a Windows clone
works today without running anything; `scripts/setup.ps1` now reproduces them
(the whisper set is byte-for-byte the same release archive), so they can be
deleted whenever you want the repository smaller.

## 5. Build and run

### Prerequisites

```bash
brew install rustup node cmake
rustup toolchain install stable --component clippy,rustfmt
```

**Homebrew's `rustup` does not put `cargo` on your PATH.** It installs the
toolchain shims under `$(brew --prefix rustup)/bin` and links only `rustup` itself
into `/opt/homebrew/bin`, so `cargo` comes back "command not found" while
`rustup --version` works fine. Add this to your shell profile:

```bash
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"
```

Xcode Command Line Tools supply the C++ compiler and the CoreAudio/AudioToolbox
headers; run `xcode-select --install` if `clang` is missing.

### Build

```bash
scripts/setup.sh              # once: the platform-specific dependencies (section 4)
scripts/build_all.sh          # HUD first, then the Rust workspace, then audio
cargo run -p oracle-core -- run --config deploy/oracle.macos.toml
```

The profile resolves `${ORACLE_ROOT}` from the `.oracle-root` marker at the top
of the checkout, and `${ORACLE_PLATFORM}` to `macos-arm64` or `macos-x64`, so it
works unedited from a fresh clone and on either Mac architecture. Set
`ORACLE_ROOT` in the environment to override.

`--alsa` is Linux-only and the script refuses it here: CMake selects the CoreAudio
backend automatically on `APPLE`, and the audio test binary prints
`capture backend: coreaudio` to prove which one was actually compiled. If it says
`null`, capture produces a test tone rather than your microphone.

### The native window

`oracle-shell` (bare tao + wry — **not** Tauri) builds and runs on macOS:

```bash
cd oracle-shell && cargo run --release
```

Its `build-app.ps1` is Windows-only, so on a Mac you build with plain cargo and
put `oracle-core` next to the resulting binary yourself (or set `ORACLE_CORE_EXE`).
The global hotkey and tray icon work. The shutdown backstop that force-kills
stragglers by name is still Windows-only, so if core wedges, `pkill oracle-actd`
is the manual equivalent.

## What works, and what does not

| Capability | macOS | Notes |
|---|---|---|
| Process list / kill | ✅ | `ps` and `kill`; no permission needed |
| Window list / focus / minimize / close | ✅ | Accessibility |
| Input injection (`type_text`) | ✅ | Accessibility |
| Open app / URL / file | ✅ | `open`; no permission needed |
| Lock screen | ✅ | `CGSession -suspend` |
| Volume up / down / mute | ✅ | Real system volume |
| Play / pause / next / previous | ⚠️ | Drives Spotify or Music directly. AppleScript cannot post the system-defined events the physical media keys use, so if neither app is running this reports an error rather than doing nothing |
| Read UI tree / click by name | ⚠️ | Implemented via Accessibility, but the least battle-tested path — see below |
| Microphone capture | ✅ | CoreAudio (AUHAL). Needs the Microphone permission — see below |
| Speech in / out / wake word | ⚠️ | Works, but the vendored `whisper/` and `piper/` are Windows binaries. Rebuild for arm64 — see §4 |
| Native window (`oracle-shell`) | ✅ | tao + wry build and run; only `build-app.ps1` is Windows-only |

### Performance

Every actuation shells out to `osascript`, which costs roughly 50-150ms per call.
That is fine for "focus that window" and wrong for anything per-frame. The whole
backend routes through one `osascript` helper in `oracle-actd/src/pal/macos.rs`,
so replacing it with native `AXUIElement`/`CGEvent` FFI is a change to that single
function rather than to the twelve trait methods.

### Window ids

The Accessibility API addresses a window by its index in an application's window
list, not by a global handle, so ids are synthesized as `(pid << 32) | index`. An
index is only stable while window order is, so an id can go stale if windows open
or close between listing and acting. Every operation re-resolves the window and
reports `NoWindow` rather than acting on whatever now sits at that index.
