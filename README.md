# Oracle of Delphi

[![CI](https://github.com/apollo-2006/oracle-of-delphi/actions/workflows/ci.yml/badge.svg)](https://github.com/apollo-2006/oracle-of-delphi/actions/workflows/ci.yml)
[![demo](https://github.com/apollo-2006/oracle-of-delphi/actions/workflows/pages.yml/badge.svg)](https://github.com/apollo-2006/oracle-of-delphi/actions/workflows/pages.yml)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A fully local voice assistant. Pythia listens for her name, plans with a 14B model on
your own GPU, acts on the machine through a separate privileged daemon that asks before
anything irreversible, remembers across sessions, and talks back. Nothing leaves the box.

**[Try the HUD in your browser →](https://apollo-2006.github.io/oracle-of-delphi/)** The
real Three.js interface, built from `oracle-v37/oracle-hud`, with a scripted stand-in for
the orchestrator: type a request and watch the state changes, tool rows, the Apollo
confirmation for an irreversible action, and a streamed, spoken reply. The actual
assistant needs a local model and GPU, so the replies on that page are fixed scripts, and
the page says so.

## Where things are

The system lives in [`oracle-v37/`](oracle-v37/). Start with its
[README](oracle-v37/README.md) for the architecture, build steps and the offline mode that
runs with no GPU, model or credentials.

```
oracle-v37/oracle-core    Rust orchestrator: agent loop, memory, tools, HUD gateway
oracle-v37/oracle-actd    Rust privileged daemon: capability policy, confirmation, audit
oracle-v37/oracle-shell   native window, hotkey and tray (tao + wry)
oracle-v37/oracle-audio   C++ capture, voice activity detection, barge-in
oracle-v37/oracle-hud     Three.js heads-up display, plus the scripted demo build
oracle-v37/oracle-ipc     shared wire types
piper/, whisper/          third-party speech binaries and models; see THIRD-PARTY-NOTICES.md
```

The design premise is that the model is an untrusted planner. Every OS-touching action
runs in `oracle-actd`, which recomputes the capability an operation needs from the
operation itself rather than from what the model claims, and parks irreversible ones
until the user sanctions them in the HUD.

A write-up of four failures from building it, none of which turned out to be the model,
is at [abirdeol.tech/research/oracle](https://abirdeol.tech/research/oracle).

## License

MIT for the code in this repository; bundled third-party components keep their own
licences, listed in [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
