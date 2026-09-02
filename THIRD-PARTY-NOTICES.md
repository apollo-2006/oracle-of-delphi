# Third-Party Notices

Oracle of Delphi is licensed under the MIT License (see [`LICENSE`](LICENSE)).
That covers **this project's own code**. It does not, and cannot, change the
terms of the third-party software distributed alongside it or used at runtime —
each component below keeps its own license wherever it is redistributed.

This file exists because the repository tracks 402 third-party binary and data
files with no license text of their own bundled anywhere.

---

## 1. Redistributed in this repository

These files are committed to git, so cloning or forking this repository
distributes them. **This is the section that carries obligations.**

### ⚠️ espeak-ng — GPL-3.0-or-later

| Path | Files |
|---|---|
| `piper/espeak-ng-data/**` | 355 |
| `piper/espeak-ng.dll` | 1 |

espeak-ng is a phonemizer, used by Piper to turn text into phonemes. It is
**copyleft** — the only component here that is. Redistributing these files
carries GPL-3.0 obligations (notably: offering the corresponding source, and
passing the same license along), regardless of what this repository's own
`LICENSE` says.

**This does not make Oracle of Delphi a derivative work.** `oracle-core` invokes
Piper as a **separate process** over stdin/stdout (`[voice] tts_program`), which
is arm's-length invocation, not linking. The MIT license on this project's own
source stands. The obligation attaches to *shipping espeak-ng's files*, not to
the code that shells out to a program that uses them.

- Upstream: https://github.com/espeak-ng/espeak-ng
- License: https://github.com/espeak-ng/espeak-ng/blob/master/COPYING

**The simplest way to not have this obligation is to stop vendoring these
files**, and that is now a safe, verified change rather than a suggestion:

* `scripts/setup.ps1` fetches `whisper-bin-x64.zip` from whisper.cpp release
  `b4938`, whose 38 `.exe`/`.dll` files are a **byte-for-byte match for the
  file list vendored in `whisper/`** — the setup script reproduces that
  directory exactly.
* It installs Piper from the `piper-tts` wheel, which supersedes the vendored
  `piper/*.exe`. Note what this does and does not do: the wheel is itself
  GPL-3.0-or-later (see below), so the obligation is **relocated, not removed**
  — from code this repository redistributes to code the user installs on their
  own machine. That is still the improvement worth having, because it is
  redistribution that puts the obligation on this project.

So `git rm -r --cached piper/espeak-ng-data piper/*.exe piper/*.dll whisper/*.exe
whisper/*.dll` would delete this entire section along with ~75 MB, and a Windows
clone would still work after one `scripts/setup.ps1`. Keep
`piper/en_US-amy-medium.onnx`: it is platform-neutral and used on every OS.

### Piper — two projects, one name

"Piper" here refers to two separately licensed projects, and an earlier version
of this file conflated them. They must be read side by side:

| | Vendored binaries | The `piper-tts` wheel |
|---|---|---|
| Project | `rhasspy/piper` | `OHF-Voice/piper1-gpl` |
| License | **MIT** | **GPL-3.0-or-later** |
| Files | `piper/piper.exe`, `piper/piper_phonemize.dll` | installed into `.venv/` by the setup scripts |
| Reaches this repo by | redistribution (committed to git) | the user running `pip install` |

Neural text-to-speech. The vendored Windows build is MIT and links the
espeak-ng above. The wheel is the maintained successor and is GPL-3.0-or-later
— it carries espeak-ng inside it rather than beside it, which is why its own
license is GPL and not MIT.

So moving to the wheel does not make the GPL question go away; it changes who
answers it. This repository stops shipping GPL-licensed files, and the user
installs them instead. Invocation is unchanged either way: Piper runs as a
separate process over stdin/stdout (`[voice] tts_program`), which is
arm's-length invocation rather than linking, so this project's own MIT license
is unaffected in both cases.

`scripts/setup.sh` and `scripts/setup.ps1` pin `piper-tts>=1.7.0`. The floor is
not cosmetic: the 1.6.1 arm64 macOS wheel baked its build machine's espeak-ng
data path into the compiled extension, so every synthesis returns a 0-byte WAV
(OHF-Voice/piper1-gpl#272, open at the time of writing; reproduced as fixed on
1.7.0). An unpinned `pip install --upgrade` that resolved to it would leave
`tts_program` producing silence, and the HUD would fall back to browser speech
with nothing in the logs to say why.

The vendored `.exe`/`.dll` are only what a Windows clone uses before the setup
script has been run. `piper/en_US-amy-medium.onnx` is the voice model and is
used on every OS.

- Vendored upstream (MIT): https://github.com/rhasspy/piper
- Wheel upstream (GPL-3.0-or-later): https://github.com/OHF-Voice/piper1-gpl
- Wheel: https://pypi.org/project/piper-tts/

### ONNX Runtime — MIT

`piper/onnxruntime.dll`, `piper/onnxruntime_providers_shared.dll`

Inference runtime for the Piper voice model.

- Upstream: https://github.com/microsoft/onnxruntime

### whisper.cpp and ggml — MIT

`whisper/whisper.dll`, `whisper/whisper-*.exe`, `whisper/ggml.dll`,
`whisper/ggml-base.dll`, `whisper/ggml-cpu-*.dll`, `whisper/main.exe`,
`whisper/stream.exe`, `whisper/bench.exe`, `whisper/command.exe`,
`whisper/wchess.exe`, `whisper/test-*.exe`, `whisper/test.wav`

Speech recognition (`whisper-cli` for transcription, `whisper-stream` for the
wake word) and the tensor library underneath it.

- Upstream: https://github.com/ggml-org/whisper.cpp

### llama.cpp — MIT

`whisper/llama.dll`

Present as a dependency of `whisper-talk-llama.exe`. The llama.cpp used for
actual inference is a **separate local checkout** — see §2.

- Upstream: https://github.com/ggml-org/llama.cpp

### SDL2 — zlib License

`whisper/SDL2.dll`

Audio capture for `whisper-stream`, which opens the microphone directly rather
than going through the HUD.

- Upstream: https://github.com/libsdl-org/SDL

### ⚠️ Components whose license was NOT verified

Listed separately rather than guessed at. **Confirm these upstream before
relying on this file.**

| File | What it is | Status |
|---|---|---|
| `piper/en_US-amy-medium.onnx` (+ `.json`) | Piper voice model, dataset `amy` | The model card carries no license field. Piper voice licenses vary **per voice** with the dataset they were trained on — check `VOICES.md` in the Piper repo for `en_US-amy-medium` |
| `piper/libtashkeel_model.ort` | Arabic diacritization model used by `piper_phonemize` | Not verified. Unused by this project (English only) — a candidate for deletion |
| `whisper/parakeet*.exe`, `whisper/parakeet.dll` | Parakeet ASR support built against ggml | Not verified. Unused by this project — a candidate for deletion |

---

## 2. Used at runtime, not redistributed

Downloaded or built by the user. `.gitignore` keeps them out of the repository,
so this project distributes none of them — but you are still bound by their
terms when you run them.

| Component | License | Notes |
|---|---|---|
| llama.cpp | MIT | Cloned and built per platform (Metal on macOS, ROCm/Vulkan on Windows). See `docs/MACOS.md` §2 |
| Qwen2.5-7B-Instruct (GGUF) | Apache-2.0 | The planner. Verify the license of whichever model you actually use — it varies by size and vendor |
| Qwen3-VL-2B-Instruct (GGUF) | Apache-2.0 | The vision tier, `[llm.small]` |
| BGE-small-en-v1.5 (GGUF) | MIT | The embedding sidecar, `[memory.embedder]` |

Model weights are **not** covered by this project's MIT license. Quantized GGUF
re-uploads may carry additional terms from whoever produced them.

---

## 3. Build and library dependencies

Not enumerated by hand here, because a hand-maintained list of several hundred
transitive crates goes stale immediately and a stale license file is worse than
none.

- **Rust:** declared in `oracle-v37/Cargo.lock` and `oracle-v37/oracle-shell/Cargo.lock`.
- **JavaScript:** declared in `oracle-v37/oracle-hud/package-lock.json`.

To generate a real, current report:

```bash
cargo install cargo-about && cargo about generate about.hbs   # attribution
cargo install cargo-deny  && cargo deny check licenses        # policy check
npx license-checker --summary                                 # HUD deps
```

---

*Last reviewed: 2026-09-01. Licenses were identified from upstream projects and
from strings embedded in the shipped binaries; entries marked ⚠️ were not
verified and should be confirmed before this file is relied upon.*
