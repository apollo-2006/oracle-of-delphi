# Oracle of Delphi — the desktop app

> **This document described a design that was replaced.**
>
> It documented `oracle-app/`, a **Tauri v2** shell with `src-tauri/`,
> `tauri.conf.json`, `capabilities/default.json`, sidecar manifests and an NSIS
> installer. None of that exists in this repository, and none of it is how the
> app is built today. It was kept verbatim long enough to send people looking
> for directories that were never committed, so it is replaced rather than
> patched.
>
> The current shell is [`oracle-shell/`](../oracle-shell/README.md): a window
> built directly on **tao + wry**, with **no Tauri framework** — no bundler, no
> `tauri.conf.json`, no capabilities file, no sidecar manifests.

## Where to actually look

| You want | Read |
|---|---|
| The native window: what it is, how to build and run it | [`oracle-shell/README.md`](../oracle-shell/README.md) |
| Starting the whole assistant with no terminals | [`ONE_CLICK.md`](ONE_CLICK.md) |
| Running it on a Mac | [`MACOS.md`](MACOS.md) |
| Running it on Windows | [`WINDOWS.md`](WINDOWS.md) |
| Day-two operations | [`RUNBOOK.md`](RUNBOOK.md) |

## What is still true

The parts of the old document that survived the change of framework, because
they were never really about Tauri:

- **The backend does not change.** `oracle-core` and `oracle-actd` are the same
  binaries the CLI runs, driven by the same `oracle.toml`. All the
  intelligence, tools, memory, Google integration and confirmation gating live
  there. The shell is deliberately dumb.
- **The HUD is unchanged.** The same Three.js HUD `oracle-core` serves at
  `http://127.0.0.1:8770` is what the window displays, over the same WebSocket
  protocol — same telemetry, same voice loop, same Apollo-decree confirm flow.
- **It is a real window, not a bundled browser.** On Windows that is the WebView2
  runtime that ships with Windows 11; on macOS it is WKWebView. Either way there
  is no second Chromium on a machine that already has one.
- **The window can do what a tab cannot:** a system-tray sun, and a global
  hotkey (`Ctrl+Alt+O`) that summons Pythia from any app. Closing the window
  hides it to the tray; a real quit shuts the whole tree down.
- **It is outside the Cargo workspace.** `oracle-shell` is `exclude`d in the root
  `Cargo.toml` because it pulls webview/GUI system dependencies, so `cargo build`
  at the root never tries to compile it and CI never needs a GUI runner.

## What changed beyond the framework

- **Build.** `oracle-shell/build-app.ps1` on Windows; plain
  `cargo build --release` inside `oracle-shell/` on macOS and Linux. There is no
  installer to produce and no `npm run stage` step — the script simply copies
  `oracle-core` and `oracle-actd` next to the shell binary so the app is one
  folder.
- **Config discovery.** The shell resolves `oracle.toml` from `ORACLE_CONFIG`,
  else a per-platform default: `%APPDATA%\oracle\` on Windows,
  `~/Library/Application Support/oracle/` on macOS, `$XDG_CONFIG_HOME/oracle/`
  (else `~/.config/oracle/`) elsewhere. The old app-specific
  `com.oracle-of-delphi.desktop` directory is gone — the shell and the CLI now
  read the same file.
- **Icons.** One `oracle-shell/icons/icon.png`, embedded at compile time with
  `include_bytes!` and decoded for both the window and the tray. There is no
  generated multi-resolution icon set and no `gen_icons.py`.
