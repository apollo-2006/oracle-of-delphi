# Threat Model

Oracle of Delphi can see your screen, type on your keyboard, run shell commands,
read your email, and open your front-door lock. That capability is the whole
point and the whole risk. This document states what we defend against, how, and
what we explicitly do not defend against.

## Assets

1. The user's machine (files, processes, input devices).
2. The user's accounts (Google tokens in the vault).
3. The user's physical environment (locks, thermostat, cameras via HA).
4. The user's data (conversation history, knowledge graph).

## Adversaries

| # | Adversary | Capability |
|---|---|---|
| A1 | **The LLM itself** | Untrusted planner. May hallucinate, be jailbroken, or be steered by injected content. |
| A2 | **Prompt injection via content** | Attacker-controlled text in an email, web page, or screen the assistant reads. |
| A3 | **A compromised `oracle-core`** | The largest attack surface (network, parsing, model). Assume it can be exploited. |
| A4 | **A local non-root process** | Another program on the machine trying to drive the daemon. |
| A5 | **Credential theft** | Anything trying to read the OAuth tokens at rest. |

## Defenses (and where they live in code)

### The LLM is never trusted with privilege (A1, A3)

The capability required by any actuation is **recomputed inside `oracle-actd`
from the operation itself** (`ActRequest::required_capability`), never taken from
the caller. A compromised core cannot under-declare a capability to bypass
policy; it can only send an op, and the daemon decides. T3 (credential stores,
policy self-modification) is not in the model's tool list at all. Tested in
`oracle-actd/src/policy.rs` and over a real socket in
`tests/socket_integration.rs`.

### Process + privilege isolation (A3)

Actuation lives in a separate daemon behind an authenticated socket. Even a full
RCE in core cannot inject keystrokes directly — it must ask actd, which
enforces policy, confirmation, and the injection denylist. The socket is
`0600` and peer-verified with `SO_PEERCRED` (same-uid only), so A4 cannot
connect. Anti-replay nonces prevent a captured RPC from being resent.

### Irreversible actions require confirmation (A1, A2)

Any op flagged irreversible (`kill_process`, full-user shell, `gmail.send`,
locks) returns `NeedsConfirmation`; it does not execute until the user says yes
(spoken, or the HUD confirm button). The confirmation is round-tripped over the
socket and audited. Tested end-to-end.

### Prompt-injection containment (A2)

All external text (email bodies, web, OCR) is wrapped in
`<<<Oracle_UNTRUSTED_DATA>>>` fences with a standing system rule that fenced
content is data, never instructions (`security::wrap_untrusted` + `DATA_RULE`,
shipped in every turn's system prompt). Forged fences inside the content are
neutralized. Additionally, **a turn that ingests new external content may not
trigger a T2 action without fresh confirmation** — the classic "email tells the
assistant to forward all your mail" attack is blocked at the actuation layer,
not just the prompt layer.

### Input-injection safety (A1)

- **Dead-man switch:** physical keyboard/mouse activity during synthetic input
  aborts the sequence within one event.
- **Panic gesture / "freeze":** puts actd in `LOCKDOWN`, disabling injection and
  shell until re-armed. Un-lockdown is the only op allowed while locked down.
- **Context denylist:** injection is refused when a password manager or a
  password field is focused — re-checked *after* focus, at execution time.

### Credentials at rest (A5)

Refresh/access tokens are sealed with AES-256-GCM. The master key lives in the
OS keyring (Secret Service / DPAPI), never on disk in plaintext. The AAD binds
each ciphertext to `provider|account|scope-hash`, so a stolen blob can't be
replayed for a different scope or account. Logs and telemetry pass through
`security::redact`, which scrubs token-shaped and key=value secrets.

### Audit (all)

Every privileged action — allowed, denied, or confirmed — is written to an
append-only journal with its turn id. "What did you do while I was gone?" is a
query, and a human can review the log independently.

## Out of scope (explicitly not defended)

- **A malicious operating system or hardware.** If the kernel or firmware is
  compromised, all bets are off.
- **A root-level local attacker.** Root can read the keyring, ptrace the
  processes, and drive `/dev/uinput` directly. We defend against non-root A4,
  not root.
- **Physical access to an unlocked session.** If someone is at your keyboard,
  they are you as far as the OS is concerned.
- **The security of the LLM's *judgment*.** We contain what the model can *do*;
  we cannot guarantee it always *decides* well. Confirmation gates on
  irreversible actions exist precisely because model judgment is fallible.
- **Supply chain of third-party crates/models.** Pin hashes; audit dependencies
  out of band.

## Residual risks we accept

- A user who stands on "confirm" for everything trains themselves to click
  through. The mitigation is scope: only genuinely irreversible actions prompt,
  so prompts stay rare enough to mean something.
- The VLM screen-perception path can be fooled by adversarial UI. It is capped
  at 6 steps and every action is verified by re-capture, but a determined
  adversarial interface is a known limitation of pixel-level automation.
