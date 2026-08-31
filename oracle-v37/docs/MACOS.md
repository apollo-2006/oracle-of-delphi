# Running on macOS (Apple Silicon)

macOS is a first-class target alongside Linux and Windows. This covers the two
things that differ: the permissions the actuator needs, and the inference backend.

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

Then point `oracle.toml` at it. `-ngl 99` offloads every layer to the GPU; unified
memory means a 14B model at Q4 wants roughly 10-12 GB, so it fits comfortably on a
32 GB machine and is tight on 16 GB (drop to an 8B model there).

```toml
[supervise]
autostart_llm = true
llm_program = "/path/to/llama.cpp/build/bin/llama-server"
llm_args = ["-m", "/path/to/qwen2.5-14b-instruct-q4_k_m.gguf",
            "--port", "8080", "-ngl", "99", "-c", "8192"]
```

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

## 4. Build and run

```bash
scripts/build_all.sh          # HUD first, then the Rust workspace, then audio
cargo run -p oracle-core -- run
```

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
