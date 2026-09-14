# Third-Party Notices

Oracle of Delphi is licensed under the MIT License (see [`LICENSE`](LICENSE)).
That covers **this project's own code**. It does not, and cannot, change the
terms of the third-party software the project uses at runtime; each component
below keeps its own license.

---

## 1. Redistributed in this repository

**Nothing, as of 2026-09-14.**

Until then the repository tracked 402 third-party binary and data files under
`piper/` and `whisper/`: Windows builds of Piper, ONNX Runtime and whisper.cpp,
SDL2, espeak-ng with its 355 data files (GPL-3.0-or-later, the one copyleft
component), an Arabic diacritization model, the whisper.cpp test binaries, and
the Piper voice model. They were removed once the setup scripts could install
everything the shipped profiles actually use. The 38 whisper.cpp binaries were
byte-for-byte the release `b4938` archive that `scripts/setup.ps1` downloads (the
39th file was a test WAV), and the profiles already ran Piper from the
`piper-tts` wheel rather than the vendored build. Both directories are now
ignored.

Commits from before that change still contain those files, so the git history
continues to carry them under the terms listed in the previous revision of this
file.

---

## 2. Installed by the setup scripts

`oracle-v37/scripts/setup.sh` (macOS, Linux) and `oracle-v37/scripts/setup.ps1`
(Windows) fetch or build these on the user's own machine. This project
distributes none of them, but you are bound by their terms when you run them.

| Component | License | How it arrives |
|---|---|---|
| Piper, the `piper-tts` wheel | **GPL-3.0-or-later** | `pip install 'piper-tts>=1.7.0'` into `.venv/` |
| Piper voice `en_US-amy-medium` (+ `.json`) | Set by its training dataset, from `MycroftAI/mimic3-voices`; **not verified here** | Downloaded from `rhasspy/piper-voices` at revision `v1.0.0` and checked against a pinned SHA-256 |
| whisper.cpp and ggml | MIT | Windows: the release `b4938` binaries. macOS and Linux: built from the `b4938` tag |
| SDL2 | zlib | Inside the whisper.cpp Windows release; a system package elsewhere (`brew install sdl2`) |
| Whisper model `ggml-base.en.bin` | MIT | Downloaded from `ggerganov/whisper.cpp` on Hugging Face |
| llama.cpp | MIT | Cloned and built per platform. See `oracle-v37/docs/MACOS.md` §2 |

### Piper: two projects, one name

The wheel is `OHF-Voice/piper1-gpl`, the maintained successor to the MIT-licensed
`rhasspy/piper` whose Windows build this repository used to vendor. It carries
espeak-ng inside it, which is why its own license is GPL rather than MIT. Moving
to the wheel did not make the GPL question go away; it changed who answers it.
The user installs GPL-licensed code rather than this repository shipping it.

Either way, this project's MIT license is unaffected: `oracle-core` runs Piper as
a **separate process** over stdin and stdout (`[voice] tts_program`), which is
arm's-length invocation, not linking.

- Wheel upstream: https://github.com/OHF-Voice/piper1-gpl
- Wheel: https://pypi.org/project/piper-tts/
- espeak-ng: https://github.com/espeak-ng/espeak-ng

Both setup scripts pin `piper-tts>=1.7.0`. The floor is not cosmetic: the 1.6.1
arm64 macOS wheel baked its build machine's espeak-ng data path into the compiled
extension, so every synthesis returns a 0-byte WAV (OHF-Voice/piper1-gpl#272,
reproduced as fixed on 1.7.0). An unpinned `pip install --upgrade` that resolved
to it would leave `tts_program` producing silence, and the HUD would fall back to
browser speech with nothing in the logs to say why.

### The voice model

The model card for `en_US-amy-medium` names its dataset,
https://github.com/MycroftAI/mimic3-voices, and says only "License: See URL".
Piper voice licenses vary per voice with the dataset each was trained on, so
confirm the terms there before depending on it. Not committing the model is
what keeps this repository out of that question.

### Models you choose

The GGUF models under `oracle-models/` are yours to pick, and the licenses below
are for the ones the profiles name. Verify whichever you actually use; terms vary
by size and vendor, and quantized re-uploads may add terms of their own.

| Model | License | Role |
|---|---|---|
| Qwen2.5-14B-Instruct (GGUF) | Apache-2.0 | The planner |
| Qwen3-VL-2B-Instruct (GGUF) | Apache-2.0 | The vision tier, `[llm.small]` |
| BGE-small-en-v1.5 (GGUF) | MIT | The embedding sidecar, `[memory.embedder]` |

Model weights are **not** covered by this project's MIT license.

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

*Last reviewed: 2026-09-14. Licenses were identified from the upstream projects;
the voice model's is marked as not verified and should be confirmed before this
file is relied upon.*
