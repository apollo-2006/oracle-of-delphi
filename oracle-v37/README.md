# Project Oracle of Delphi

A fully-local autonomous voice assistant: sub-600ms bidirectional speech with
barge-in, OS-level machine control, Google Workspace + Home Assistant
automation, dual-layer persistent memory with a knowledge graph, and a WebGL
holographic HUD. Targets **Linux, macOS and Windows** from one codebase (a
Platform Abstraction Layer covers the OS-specific bits); GPU inference runs on
**AMD ROCm/HIP**, **Apple Metal**, or any OpenAI-compatible llama.cpp server.

This repository is a **working, buildable, tested system** — not a sketch.
Every component builds, runs, and passes tests; the whole thing boots offline
(mock LLM, hashing embedder, Null audio) so you can run it with no GPU, no model
download, and no credentials, then swap in real backends behind traits.

**Status: 409 Rust tests + 933 C++ checks passing. Clippy-clean, rustfmt-clean.
Two processes talk over a real authenticated socket; the HUD streams over a real
WebSocket; OAuth, Home Assistant, and the audio ring are exercised end-to-end
against mocks or real libraries.**

## Architecture

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
| `oracle-core` | Rust | ReAct agent loop; **parallel dependency-DAG dispatcher**; **two LLM tiers** (on-demand planner + resident VLM); LLM backends (mock + llama-server SSE, text and multimodal); SQLite episodic+vector+KG memory with **tagged vector spaces**; **BGE embedding sidecar**; **ambient screen index**; **knowledge-graph consolidation**; **idle work window** with GPU-pressure gating; **live WebSocket HUD gateway**; **OAuth2 PKCE loopback + AES-GCM vault**; **Home Assistant WS + MQTT clients**; config; observability + `doctor`; lifecycle/shutdown; prompt-injection hardening | 281 |
| `oracle-actd` | Rust | Capability policy; **real UDS server**; anti-replay; confirmation flow; PAL (mock + `/proc` Linux + Windows + macOS); **window capture** (GDI / `screencapture`); shell risk classifier; audit journal | 68 |
| `oracle-audio` | C++20 | Lock-free SPSC ring; VAD/barge-in state machine; TTS flow control + heard-upto mapping; FIR decimator; **real ALSA / WASAPI / CoreAudio capture backends** | 933 checks |
| `oracle-hud` | TS/Three.js | Instanced audio-reactive core; EffectComposer post chain; binary WS protocol; glass panels | tsc + vite |

## Quick start (offline, no GPU)

Nothing below needs editing after a clone. The shipped profiles address the
checkout as `${ORACLE_ROOT}` (found via the `.oracle-root` marker) and this
machine as `${ORACLE_PLATFORM}`, so the same file works on any Mac or Windows
box — and `ORACLE_ROOT` in the environment overrides it.

```bash
# Once per machine: the platform-specific dependencies -- piper, whisper.cpp,
# the whisper model and llama.cpp. Windows: .\scripts\setup.ps1
# Skip it if you only want the offline demo below; it is needed for voice.
scripts/setup.sh

# Build + test everything (add --release --alsa for production build)
scripts/build_all.sh
# Note: the HUD must be built before the Rust workspace. oracle-core embeds
# oracle-hud/dist via #[derive(RustEmbed)], resolved at compile time, so on a
# clean checkout `cargo build` fails until `vite build` has produced that folder.
# build_all.sh does this in the right order; if you build by hand, run
#   ( cd oracle-hud && npm install && npx vite build )
# first.

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

Platform profiles live in `deploy/`: `oracle.macos.toml`, `oracle.windows.toml`
and `oracle.windows.ambient.toml`. Check one before a long build with
`cargo run -p oracle-core -- check-config --config deploy/oracle.macos.toml`.

The REPL runs the architecture's headline example end-to-end: the model emits
four tool calls, the dispatcher runs three in parallel and gates the draft on
the email + calendar results via `$result.N` dependency edges, then speaks a
summary — all real code paths, zero external dependencies.

## Continuity

Memory used to be reachable only through the `memory.remember` and
`memory.recall` tools — that is, only when the *model chose* to call them. A 14B
planner almost never does either unprompted, so nothing was written and nothing
was read back: the store stayed empty and the assistant met you fresh every
session. The infrastructure was all there; nothing was driving it.

It is now automatic on both sides of every turn:

* **Before planning**, the user's turn is embedded, the store is searched, and
  anything clearing `memory.recall_min_score` is rendered into the system prompt
  with a human-readable age ("3 days ago"). No tool call required.
* **After the turn resolves**, the user's words and the reply are written back as
  episodes, so the next turn has something to find. The user's half carries
  higher salience — durable facts come from them, not from Pythia's restatement.
* **An exact repeat reinforces** the existing episode instead of appending a
  near-duplicate, so saying the same thing twice deepens a memory rather than
  crowding the recall block with copies.

Recalled text is framed in the prompt as **data, not instructions**. A memory can
quote an email or a web page, so an injected instruction could otherwise reach
the planner a session after it was first seen — the standing `DATA_RULE` has to
cover memory, not just freshly fetched content.

Tunable under `[memory]`; set `auto_recall`/`auto_record` false to return to the
old tool-only behaviour:

```toml
[memory]
auto_recall = true
auto_record = true
recall_limit = 5
recall_min_score = 0.15
```

Retrieval quality is bounded by whichever `Embedder` is configured. The offline
`HashEmbedder` matches on tokens rather than meaning, which is why
`[memory.embedder]` exists — see [Semantic recall](#semantic-recall).

A correction worth stating plainly, because an earlier version of this document
had it wrong: swapping embedders is **not** a free migration. Both produce 384-d
unit vectors, so old and new rows are indistinguishable as data — but the cosine
between a hashed vector and a BGE vector is noise that happens to land in
[-1, 1]. Retrieval would keep working, keep returning results, and keep being
wrong. Rows are therefore tagged with the vector space that produced them.

## What it costs when you are not using it

A 14B at Q4 holds 10-12 GB of VRAM, and a desktop assistant is overwhelmingly
idle. The wake-word recognizer is a separate small process that does not need the
model at all, so there is no reason for the planner to be resident between
conversations.

After ten minutes of silence the supervised `llama-server` is **killed**, not
just ignored — releasing the VRAM is the whole point. Saying "Delphi" starts the
reload immediately, while you are still talking, so the load overlaps with the
rest of your sentence instead of being dead air after it. The turn then waits on
`/health` before its request goes out, so the first ask after a lull is slow
rather than a connection error.

```toml
[supervise]
idle_unload_secs = 600        # 0 keeps the planner resident
llm_ready_timeout_secs = 90
```

### Two tiers, because one model was the wrong shape

That policy was correct and it was also a trap. With only a 14B, continuous
background work was impossible — you will not hold 11 GB all day to summarize a
window — so there *was* no continuous background work, so the model had nothing
to do between turns, so unloading it was the only sane answer. An assistant whose
model is usually unloaded is one that does nothing when you are not looking at
it, which is a strange thing to run locally at all.

Splitting the tier breaks the cycle:

* **Big** (`[llm]`) — the 14B planner. On demand for a turn or the away briefing,
  unloaded when idle. Nothing about its lifecycle changed.
* **Small** (`[llm.small]`) — a 2B-class VLM, ~2.5 GB, **resident**. Cheap enough
  to leave running, which is what lets it do work that arrives continuously:
  reading the screen, folding episodes into the graph.

Two `llama-server` processes on two ports, not two modes of one server —
llama.cpp holds one model per server, and the whole point is that one can die
while the other lives. Sharing a port is rejected at config load, because the
failure is otherwise silent: the second server never binds, the supervisor
restart-loops it forever, and every "small tier" request is quietly answered by
the 14B. That looks exactly like the feature working, at 11 GB resident.

```toml
[llm.small]
enabled = true
backend = "http://127.0.0.1:8081"   # its own port
model = "qwen3-vl-2b-instruct-q4_k_m"
resident = true                     # never idle-unloaded — that is the point
```

## The work window

Idleness used to mean "unload the planner and do nothing", so the GPU sat free at
exactly the moment nothing was queued for it. With a resident small tier, idle
becomes the window in which the backlog runs.

Background work is gated on three things, and the third is the one that matters:

- **the user is idle** — not because the work is expensive, but because it is not
  urgent; anything that can wait should
- **no turn is in flight** — idle-by-clock and busy-by-turn overlap, since a
  routine or a briefing runs unattended
- **nothing else wants the GPU** — polled from `nvidia-smi` / `rocm-smi`, minus
  our own footprint

An **unknown** GPU answer closes the window. Wrongly idling costs a late
summary; wrongly running costs you the frame rate in whatever you just launched.
Fail toward the recoverable mistake.

```toml
[workwindow]
after_secs = 900
foreign_vram_budget_mb = 1024   # a compositor holds a few hundred MB; a game, GBs
own_vram_mb = 3000
```

## Semantic recall

`HashEmbedder` matches tokens, so "the borrow checker complaint in dispatch.rs"
does not retrieve "lifetime error in the dispatcher". Pointing `[memory.embedder]`
at a llama.cpp sidecar serving BGE-small (`--embedding --pooling mean`) is what
makes recall semantic. It is a third supervised child rather than an in-process
ONNX Runtime: the supervision, restart and logging already exist, and
`oracle-core` keeps a native-dependency surface of zero.

Every row records the vector space that wrote it. Cosine is only ever taken
within one space; a row from another scores nothing and is excluded from the
vector rank list entirely. **Keyword retrieval stays space-independent**, so
switching embedders does not make history vanish — it makes it findable by words
but not by meaning, and startup prints how many rows are in that state. A switch
is something you are told about, rather than something you experience as "she
forgot everything".

```toml
[memory.embedder]
enabled = true
backend = "http://127.0.0.1:8082"   # a third port
model = "bge-small-en-v1.5"
dim = 384                           # a mismatch is refused, not stored
```

## The ambient index

Everything else in this codebase reacts. You ask, it answers; a trigger fires, it
speaks. That shape is why a local model was hard to justify — a reactive
assistant uses its GPU for seconds a day, on small inputs, competing with a cloud
model that is better at exactly that.

This is the inverse workload, and the one a cloud model cannot have: a continuous
stream of private data that never leaves the machine. The focused window is
sampled, the resident VLM reads each frame, and what it saw becomes searchable
memory. "What was that crate I was reading about on Tuesday" stops being a
question the assistant cannot answer.

**Capture and interpretation are separate tasks with a bounded queue between
them**, because they want opposite conditions. Capture must happen while you are
*working* — that is when the screen has anything on it — and is nearly free: a
`StretchBlt` and a PNG encode, no GPU. Interpretation is the expensive half and
can happen whenever; if the GPU is busy, the queue waits. Fusing them forces one
condition to win and either choice is bad: tie interpretation to capture and it
competes with your game; tie capture to idleness and it only photographs an empty
desktop.

Frames are **never written to disk**. They live in a bounded in-memory queue, go
to the model, and are dropped. What persists is text.

The screen is now the most attacker-controlled input in the system — a web page
renders whatever text it likes, and that reaches the VLM, whose summary reaches
the planner via recall a session later. Three things contain it: the VLM has **no
tools** (the same boundary as [Proactive nudges](#proactive-nudges)), its prompt
states that screen text is data being described rather than instructions, and
observations land in the memory store whose recall block already carries the
standing `DATA_RULE`.

Capture is `Capability::Observe` in actd, alongside `ReadUiTree` — both read
window contents without touching anything — and lockdown denies it with
everything else. Enabling it without `[llm.small]` is a **load-time error**:
capturing the screen every 45 seconds for a model that does not exist is all of
the privacy cost and none of the benefit.

```toml
[ambient]
enabled = false               # you switch this on; you do not discover it
sample_secs = 45
change_threshold = 6          # Hamming distance that counts as a new screen
interpret_while_active = true
retain_days = 21
```

Platform support is honest: **Windows** is complete (GDI `StretchBlt`, scaling
during the blit so a 4K window never materializes as 32 MB). **macOS** captures
the window rectangle via `screencapture -R` — a region grab wearing a window
grab's name, so an overlapping window is included; it needs the Screen Recording
grant. **Linux** returns `Unsupported`: X11 and Wayland are different enough that
one working is not the other working.

## Consolidation

`kg_node` and `kg_edge` have existed since the first commit, and nothing ever
wrote to them outside a tool call the planner almost never makes. So the graph
stayed empty and every fact lived or died with its episode.

The consolidation pass populates it: pending episodes go to the small tier, which
returns the durable relations they establish, and those are asserted into the
graph. Output is GBNF-constrained to a well-formed fact array whose relation is
drawn from the graph's own vocabulary — the same trick that makes tool calls
reliable — so malformed output stops being a failure mode.

This is what makes `ambient.retain_days` a **promotion deadline** rather than a
plain delete. Observations are mined and then swept; the knowledge persists.

Two behaviours worth knowing:

- A batch that yields no facts is still **marked read**. Yielding nothing is an
  answer, not a reason to re-read the same barren rows forever while new ones
  queue behind them.
- A **failed model call is not**, so a sidecar that is down defers the work
  instead of silently burning through the backlog.

Every edge records provenance including whether the batch touched the screen, so
a fact derived from a web page stays distinguishable from one the user said
aloud. The vocabulary is fixed and the model has no tools, but a page can still
try to get a *plausible* relation asserted; `from_observations = false` removes
that source entirely. It is a bounded risk, not a solved one.

```toml
[consolidate]
enabled = false
batch_size = 12
from_observations = true
```

## Ambient screen context

Distinct from [the ambient index](#the-ambient-index) above, and worth keeping
straight: this is window *titles* in the system prompt on every turn, costing
nothing and needing no model. The index is *pixels* read by a VLM in the
background. This one answers "close this"; that one answers "what was I reading
on Tuesday".

The assistant used to be blind unless the model chose to call a screen tool
first, which made "close this" and "what does this error mean" unanswerable. The
focused window — and a few other open ones — now go into the system prompt every
turn, the same way recalled memory does.

The subtlety: when you talk through the HUD, **Oracle is the foreground window**.
Reading that back is how a model starts describing Pythia's own UI as if it were
your screen. Windows arrive in z-order, so the first real one behind us is what
you were actually looking at.

Window titles are attacker-controllable — a web page picks its own — so the block
carries the same DATA-not-instructions framing as memory.

```toml
[agent]
screen_context = true
screen_other_windows = 6
```

## Standing orders

Things you ask for once that should keep happening:

> "Every weekday at half eight, tell me my first meeting."

Stored in the same SQLite file as memory, managed by voice through
`routine.add` / `routine.list` / `routine.remove`. The schedule vocabulary is
deliberately tiny, because cron is unreadable out loud: `daily HH:MM`,
`weekdays HH:MM`, `every Nm`, `every Nh`.

A due routine is injected into the ordinary command channel, so it gets history,
TTS, the HUD state machine and reload-on-demand for free, and behaves exactly as
if you had typed it at that moment.

### Why routines run the planner and nudges do not

A nudge is a heuristic firing on its own; a routine is *your own instruction,
time-shifted*. Running it executes a request you made rather than acting on a
guess. The capability gate is unchanged either way: an unattended turn that
reaches an irreversible action stops at the confirmer, and with nobody there to
answer it times out and is denied. A routine can read your calendar at 08:30; it
cannot quietly send mail on your behalf.

## The away briefing

Come back after a couple of hours, say "Delphi", and instead of "Yes?" you get:

> *"Your build failed — borrow checker in dispatch.rs. Three emails, the one from
> your advisor wants a reply by Friday. Your 3pm moved to 4."*

This is the one proactive path where the model earns its keep, and the split is
the point:

* **Detection stays deterministic.** What happened is gathered by ordinary Rust —
  processes that exited, mail that arrived, events on the calendar. No judgment,
  nothing to get wrong.
* **Interpretation is the model's job.** Turning three facts into the two that
  matter is what a cron cannot do, and doing it over your own machine and mail is
  what a cloud model cannot do privately.

Every other nudge in this codebase would run identically with the LLM
uninstalled. This one would not exist.

The model gets **no tools** here — it receives facts and returns prose, so the
boundary from [Proactive nudges](#proactive-nudges) holds: it cannot act, and the
worst case is an awkward sentence.

Machine events are recorded into a bounded in-memory log *before* the nudge
policy sees them, so something that happened during quiet hours is still in the
briefing even though it was never announced at the time.

Nothing to report means silence, and the check happens **before** the model call —
waking an 11 GB model to be told there is nothing to say is the opposite of the
point.

```toml
[briefing]
enabled = true
after_secs = 1200        # only after a real absence; 20 min
cooldown_secs = 1800     # not twice in half an hour
include_mail = true
include_calendar = true
lookahead_minutes = 120
```

`briefing.catch_up` exposes the same machine events on demand ("what did I
miss?"), and a routine can schedule one.

## Proactive nudges

Pythia can speak first: a calendar event about to start, or mail worth knowing
about. Off by default — an assistant that begins talking on its own is something
you opt into, not something you discover.

```toml
[proactive]
enabled = true
poll_secs = 60
lead_minutes = 10          # announce an event this far ahead
calendar = true
mail = false               # opt-in on top
mail_query = "is:unread is:starred"
watch_processes = ["cargo", "MSBuild"]   # "your build finished"
quiet_from_hour = 22       # local hours
quiet_until_hour = 8
repeat_after_secs = 21600  # don't say the same thing twice in 6h
max_per_hour = 4
```

### The planner is deliberately not in this loop

Every other path runs with the user present: they asked, they hear the answer,
and an irreversible act stops for their sanction. A proactive turn breaks all
three assumptions — it fires with nobody watching, possibly with nobody in the
room.

So there is no `Agent` and no tool registry in `oracle-core/src/proactive/`. A
trigger is ordinary Rust that reads a source and returns a fully-phrased line;
the loop's only output is speech. **The worst case for a bug here is Pythia
saying something silly at the wrong moment, never taking an unattended action.**
That is a boundary, not a default to relax.

Phrasing through the LLM would sound better and could be done safely with an
empty tool registry. It is not done yet — today's nudges are templated.

### The local triggers are the ones that justify running this at all

Calendar and mail are things your phone already does better. `watch_processes`
is not: "your build finished" needs something living on the machine, watching the
process table. The first poll only establishes a baseline, or every watched
process that finished before Oracle started would be announced at launch.

### The judgment is in the policy, not the triggers

An assistant that interrupts badly is worse than one that never speaks. The
triggers are trivial; `NudgePolicy` is where the work is:

- **quiet hours**, wrapping midnight correctly (22 → 08 is not a range test)
- **a per-nudge cooldown** keyed on the *event id*, never the time — the loop
  re-polls every 60s and rediscovers the same meeting each cycle
- **a rolling hourly ceiling**, so a misbehaving trigger cannot become a stream
- **suppression while a real turn is in flight**, released through a `Drop` so a
  panicking turn can't mute her permanently

A suppressed nudge does not consume the hourly budget, or a trigger firing at
03:00 would silently eat the morning's allowance.

## Going to production

Everything offline swaps to real backends behind a trait — nothing is stubbed
*structurally*. Full instructions in [`docs/DEPLOYMENT.md`](docs/DEPLOYMENT.md):

- **Real LLM:** point `llm.backend` at a llama.cpp server built with the HIP
  backend (`-DGGML_HIP=ON`). The SSE + tool-call protocol is already spoken.
- **Real embeddings:** set `[memory.embedder]` and point it at a llama.cpp
  sidecar serving BGE-small with `--embedding --pooling mean` on its own port.
  See [Semantic recall](#semantic-recall) — the schema matches, but existing rows
  are in the old vector space and are reported as such at startup.
- **The vision tier:** set `[llm.small]` and point it at a second llama.cpp
  server with a VLM and its `--mmproj`. This is what powers
  [the ambient index](#the-ambient-index) and [consolidation](#consolidation);
  both refuse to start without it.
- **Real audio:** Windows (WASAPI) and macOS (CoreAudio/AUHAL) build their
  backend automatically. On Linux pass `-DOracle_WITH_ALSA=ON`. A test now
  asserts the factory returns the backend the build asked for, because that
  silently regressed once: CMake defined `Oracle_WITH_ALSA` while the source
  tested `ORACLE_WITH_ALSA`, so the flag linked libasound, compiled the Null
  backend, and fed the VAD a 220Hz test tone instead of the microphone.
- **Real OS control:** the actd `Platform` trait. Windows is complete (Win32 +
  UI Automation + GDI capture). macOS is complete bar the caveats in
  [`docs/MACOS.md`](docs/MACOS.md) (Accessibility API via `osascript`; capture
  additionally needs the Screen Recording grant; the `whisper/` and `piper/`
  binaries vendored here are Windows-only and must be rebuilt for arm64 before
  the voice stack works). On Linux the `/proc` process
  lister works, the window/input backends (x11rb, `/dev/uinput`) still slot in,
  and `capture_window` returns `Unsupported` until an X11 or Wayland backend
  exists.
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

The same applies to everything added for the ambient stack, and it is worth
being specific about where the line falls. **Tested by execution:** scaling
arithmetic, PNG header parsing, `screencapture` argument construction,
perceptual hashing and change detection, queue overflow behaviour, the capture
wire type round-tripping over a real socket against the mock platform, the
consolidation pass end-to-end against a real SQLite store and graph with a
stubbed model, and every config rule. **Written and compiled but never run:**
the Windows GDI blit (it only builds under `cfg(windows)`), `screencapture`
actually returning a frame and the Screen Recording TCC prompt, any real VLM
call, and the BGE sidecar returning real vectors.

## Repository layout

```
oracle-ipc/     shared wire types + async socket transport
oracle-core/    orchestrator: agent, memory, ambient, consolidate, tiers,
                workwindow, proactive, idle, connectors, gateway
oracle-actd/    privileged actuator daemon
oracle-audio/   C++ real-time audio engine
oracle-hud/     Three.js holographic HUD
oracle-shell/   native window (tao + wry); outside the cargo workspace
deploy/         systemd units, container config, Windows + macOS profiles
docs/           DEPLOYMENT, MACOS, WINDOWS, ONE_CLICK, THREAT_MODEL, RUNBOOK
scripts/        build_all.sh, setup.sh, setup.ps1
```

## License

MIT — see [`LICENSE`](../LICENSE).

Third-party components shipped in or used by this project keep their own
licenses; they are enumerated in
[`THIRD-PARTY-NOTICES.md`](../THIRD-PARTY-NOTICES.md). One is copyleft: the
`piper/espeak-ng-data/` files and `espeak-ng.dll` vendored at the repository
root are **GPL-3.0-or-later**. That does not affect this project's own MIT
licensing -- Piper is invoked as a separate process, not linked -- but
redistributing those files carries espeak-ng's terms.
