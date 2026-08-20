# Project Oracle of Delphi

A fully-local autonomous voice assistant: sub-600ms bidirectional speech with
barge-in, OS-level machine control, Google Workspace + Home Assistant
automation, dual-layer persistent memory with a knowledge graph, and a WebGL
holographic HUD. Targets **Linux and Windows** from one codebase (a Platform
Abstraction Layer covers the OS-specific bits); GPU inference runs on **AMD
ROCm/HIP** or any OpenAI-compatible llama.cpp server.

This repository is a **working, buildable, tested system** — not a sketch.
Every component builds, runs, and passes tests; the whole thing boots offline
(mock LLM, hashing embedder, Null audio) so you can run it with no GPU, no model
download, and no credentials, then swap in real backends behind traits.

**Status: 134 Rust tests + 916 C++ checks passing. Clippy-clean, rustfmt-clean.
Two processes talk over a real authenticated socket; the HUD streams over a real
WebSocket; OAuth, Home Assistant, and the audio ring are exercised end-to-end
against mocks or real libraries.**

## Architecture

The full design document is [`oracle-architecture.md`](oracle-architecture.md).
Five crash-isolated processes:

```
oracle-audio (C++/RT)  ──shm+socket──▶  oracle-core (Rust/Tokio)  ──WS──▶  oracle-hud (Three.js)
   capture · VAD ·                          agent loop · memory ·
   barge-in · TTS                           connectors · gateway
                                                  │
                                          authed UDS (SO_PEERCRED)
                                                  ▼
                                        oracle-actd (Rust, privileged)
                                        policy · input · shell · audit
```

The security spine: the LLM is treated as an untrusted planner. Actuation lives
in a separate daemon that recomputes the required capability from the operation
itself, gates irreversible actions behind confirmation, and audits everything.
See [`docs/THREAT_MODEL.md`](docs/THREAT_MODEL.md).

## What's implemented

| Component | Language | Highlights | Tests |
|---|---|---|---|
| `oracle-ipc` | Rust | Wire types; **length-framed async transport** (UDS + peer-cred auth); binary HUD frames | 10 |
| `oracle-core` | Rust | ReAct agent loop; **parallel dependency-DAG dispatcher**; LLM backends (mock + llama-server SSE); SQLite episodic+vector+KG memory; **live WebSocket HUD gateway**; **OAuth2 PKCE loopback + AES-GCM vault**; **Home Assistant WS + MQTT clients**; config; observability + `doctor`; lifecycle/shutdown; prompt-injection hardening | 95 |
| `oracle-actd` | Rust | Capability policy; **real UDS server**; anti-replay; confirmation flow; PAL (mock + `/proc` Linux); shell risk classifier; audit journal | 29 |
| `oracle-audio` | C++20 | Lock-free SPSC ring; VAD/barge-in state machine; TTS flow control + heard-upto mapping; FIR decimator; **real ALSA capture backend** | 916 checks |
| `oracle-hud` | TS/Three.js | Instanced audio-reactive core; EffectComposer post chain; binary WS protocol; glass panels | tsc + vite |

## Quick start (offline, no GPU)

```bash
# Build + test everything (add --release --alsa for production build)
scripts/build_all.sh

# Run the orchestrator with the HUD gateway; Ctrl-C drains gracefully
cargo run -p oracle-core -- run

# Or the interactive text REPL (whole agent loop, no audio)
echo "check my advisor's email, find 30 min tomorrow afternoon, draft a reply, dim my lights" \
  | cargo run -p oracle-core -- repl

# Latency budget report
cargo run -p oracle-core -- doctor

# Actuator daemon over a real socket + audit log
oracle-actd --serve /tmp/actd.sock
```

The REPL runs the architecture's headline example end-to-end: the model emits
four tool calls, the dispatcher runs three in parallel and gates the draft on
the email + calendar results via `$result.N` dependency edges, then speaks a
summary — all real code paths, zero external dependencies.

## Going to production

Everything offline swaps to real backends behind a trait — nothing is stubbed
*structurally*. Full instructions in [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md):

- **Real LLM:** point `llm.backend` at a llama.cpp server built with the HIP
  backend (`-DGGML_HIP=ON`). The SSE + tool-call protocol is already spoken.
- **Real embeddings:** implement `memory::Embedder` with BGE-small via ONNX
  Runtime — the 384-d schema already matches, no migration.
- **Real audio:** build with `-DOracle_WITH_ALSA=ON` (verified to compile and
  link against libasound); the ring/VAD/TTS logic is unchanged.
- **Real OS control:** the actd `Platform` trait; the Linux `/proc` process
  lister already works, window/input backends (x11rb, `/dev/uinput`) slot in.
- **Real Google/HA:** the OAuth PKCE loopback, AES-GCM vault, HA WebSocket, and
  MQTT clients are complete and tested against mocks — add your client id and
  tokens.

Deploy with the provided `Dockerfile`, systemd user units
(`deploy/systemd/`), and GitHub Actions CI (`.github/workflows/ci.yml`).
Operations guidance in [`docs/RUNBOOK.md`](docs/RUNBOOK.md).

## The honest boundary

What is proven here: the architecture holds together as a real distributed
system, the tricky algorithms are implemented and tested, and every seam to a
real backend is a trait with a working mock on the other side.

What still needs the target hardware to validate: RT-audio latency under load on
a real sound card, ROCm/HIP kernels on an AMD GPU, and live Google/Home
Assistant round-trips with real credentials. The code for these paths is written
and compiles (ALSA links against libasound; the llama-server and HA clients
speak the real protocols); they are wired but not hardware-validated in CI.

## Repository layout

```
oracle-ipc/     shared wire types + async socket transport
oracle-core/    orchestrator: agent, memory, connectors, gateway, config, observ
oracle-actd/    privileged actuator daemon
oracle-audio/   C++ real-time audio engine
oracle-hud/     Three.js holographic HUD
deploy/         systemd units, container config
docs/           DEPLOYMENT, THREAT_MODEL, RUNBOOK
scripts/        build_all.sh
oracle-architecture.md   the full design document
```

## License

MIT.
