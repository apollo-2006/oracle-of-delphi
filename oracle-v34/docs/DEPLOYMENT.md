# Deployment Guide

This guide takes Oracle of Delphi from a built binary to a running assistant on a
Linux or Windows workstation, including the parts that need real hardware and
live credentials.

## 0. What runs where

| Process | Runs on | Needs |
|---|---|---|
| `oracle-actd` | the workstation | `/dev/uinput` (input), a display server (windows), user privileges |
| `oracle-core` | the workstation (or a container) | the actd socket, optionally a llama-server URL |
| `oracle-audio` | the workstation | the sound device, RT scheduling limits |
| `oracle-hud` | a browser on the workstation | the core WebSocket gateway |
| llama-server | the workstation GPU | ROCm/HIP (AMD) or CUDA |

The container image (`Dockerfile`) runs `core` + `actd` on the CPU/mock path for
CI and headless smoke tests. The full voice experience runs natively because it
needs the sound card, the GPU, and the display server.

## 1. Build

```bash
# Everything, release, with the real ALSA backend:
scripts/build_all.sh --release --alsa
# Binaries land in target/release/ and oracle-audio/build/
```

Install to `~/.local/bin`:

```bash
install -Dm755 target/release/oracle-core  ~/.local/bin/oracle-core
install -Dm755 target/release/oracle-actd  ~/.local/bin/oracle-actd
install -Dm755 oracle-audio/build/oracle-audio ~/.local/bin/oracle-audio
```

## 2. Configure

```bash
oracle-core write-config ~/.config/oracle/oracle.toml
$EDITOR ~/.config/oracle/oracle.toml
```

Key fields:
- `llm.backend` — `"mock"` to boot with no model, or a llama-server URL
  (`http://127.0.0.1:8080`).
- `actd.socket` — must match the actd `--serve` path (the systemd units already
  align these under `$XDG_RUNTIME_DIR/oracle/`).
- `actd.grant_sensitive` — leave `false`; T2 actions then require spoken/HUD
  confirmation per action.
- `hud.token` — leave empty to auto-generate a per-launch token.

## 3. The local LLM (AMD / ROCm)

Oracle of Delphi talks to any OpenAI-compatible llama.cpp `server`. Build it with
the HIP backend:

```bash
# in a llama.cpp checkout, on an AMD GPU with ROCm installed:
cmake -B build -DGGML_HIP=ON -DAMDGPU_TARGETS=gfx1100  # match your arch
cmake --build build -j
./build/bin/llama-server \
  -m qwen2.5-14b-instruct-q5_k_m.gguf \
  --host 127.0.0.1 --port 8080 \
  --ctx-size 32768 --parallel 1 --cont-batching
```

Then set `llm.backend = "http://127.0.0.1:8080"` and
`llm.model = "qwen2.5-14b-instruct"`. On Windows without ROCm, run llama-server
with the Vulkan backend (`-DGGML_VULKAN=ON`); the core doesn't care which.

### Embeddings

The reference build uses a dependency-free hashing embedder so memory works
offline. For production semantic recall, run an ONNX embedding model
(BGE-small-en-v1.5, 384-d — the DB schema already matches) and implement the
`memory::Embedder` trait against ONNX Runtime (ROCm EP on AMD). No schema
migration is needed.

## 4. Real-time audio permissions (Linux)

The audio engine wants `SCHED_FIFO` and locked memory. Add to
`/etc/security/limits.d/audio.conf`:

```
@audio   -  rtprio     95
@audio   -  memlock    unlimited
```

Add your user to the `audio` group and to a group that can open `/dev/uinput`
(often `input`), then re-login. Without these the engine still runs, just at
normal priority — fine for testing, not for tight barge-in latency.

### Wayland vs X11

Input injection and screen capture differ by session type. On Wayland, actd uses
the XDG Desktop Portal (capture) and `/dev/uinput` (injection); on X11 it uses
EWMH + XTEST. actd detects the session at startup and reports degraded
capabilities honestly to core, so the planner knows what it can't do before
planning. Grant the portal permission on first capture.

## 5. Google Workspace

1. Create an OAuth **Desktop app** client in Google Cloud Console (no client
   secret is needed for the PKCE native-app flow).
2. Put the client id in your config (a `[google]` section — wire it to
   `connectors::oauth_flow`).
3. On first use, Oracle of Delphi opens your browser, you consent, and the loopback
   server captures the code and seals the tokens into the OS keyring via the
   AES-GCM vault. Refresh is automatic at 80% of token lifetime.

Requested scopes are minimal and incremental: `gmail.modify`, `calendar.events`,
`tasks`, `contacts.readonly`.

## 6. Home Assistant + MQTT

- **Home Assistant:** create a Long-Lived Access Token (Profile → Security) and
  point the HA client at `ws://<ha-host>:8123/api/websocket`. The client
  authenticates, subscribes to `state_changed`, and mirrors entity states
  locally so reads are instant.
- **MQTT:** set `MqttConfig` host/port and subscribe topics (e.g.
  `esphome/+/state`). TLS is supported via rumqttc; commands publish at QoS 1.

## 7. Run

### systemd (recommended)

```bash
cp deploy/systemd/*.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now oracle-actd oracle-core oracle-audio
journalctl --user -u oracle-core -f
```

### Manually

```bash
oracle-actd --serve "$XDG_RUNTIME_DIR/oracle/actd.sock" &
oracle-core run --config ~/.config/oracle/oracle.toml &
oracle-audio &
```

### The HUD

```bash
cd oracle-hud && npm run build && npm run preview
# open the printed URL, appending the token core logged at startup:
#   http://localhost:4173/?ws=ws://127.0.0.1:8770/hud?token=<token>
```

## 8. Verify

```bash
oracle-core doctor        # latency budget report
journalctl --user -u oracle-actd | tail   # audit journal of privileged actions
```

## 9. Containerized (headless / CI)

```bash
docker build -t oracle .
docker run --rm -p 8770:8770 oracle-core run --config /etc/oracle/oracle.toml
docker run --rm oracle-core doctor
```

The container runs the mock LLM and Null audio backend — it proves the
orchestrator, gateway, memory, and tool loop boot and serve, without a GPU or
sound card.
