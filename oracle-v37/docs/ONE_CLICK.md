# Oracle of Delphi — one-click startup (no terminals)

This is the recommended way to run the Oracle. There is **no Tauri app to build**
and **no PowerShell windows to keep open**. You double-click one thing; the whole
assistant comes up hidden in the background and its face opens in a chromeless
window. Double-click it again and it just summons the existing instance.

## How it works

`oracle-core` now does the orchestration itself:

- It **serves the HUD**. The built frontend (`oracle-hud/dist`) is embedded
  straight into `oracle-core.exe`, so core answers plain browser requests on its
  gateway port. Open `http://127.0.0.1:8770/` and the full Oracle loads — no
  vite, no static server, no desktop shell.
- It **supervises its dependencies**. On `run`, core launches the LLM server and
  `oracle-actd` as *hidden* child processes (no console windows), restarts them
  if they die, and kills them when it exits. Their logs go to
  `%APPDATA%\oracle\run\{llm,actd}.log`.
- It **opens the face**. Core launches a chromeless Edge/Chrome app window
  pointed at the HUD. Because it's real Edge/Chrome (not an embedded webview),
  the Web Speech API — voice in *and* voice out — works.
- It's **single-instance**. If the gateway port is already served, core knows an
  Oracle is already running and just opens the window instead of starting a
  second, clashing copy. (Two cores fighting over the port/pipe was the cause of
  the old "connection drops" flakiness.)

So the process tree is: **one launcher → oracle-core → (llama-server, actd) +
the app window.** Zero visible terminals.

## One-time setup

1. Build the pieces (once, and after any code change):

   ```powershell
   npm --prefix oracle-hud install        # first time only
   npm --prefix oracle-hud run build      # build the HUD so it embeds into core
   cargo build --release -p oracle-core -p oracle-actd
   ```

   Order matters: build the HUD *before* core, so the freshest HUD is embedded.

2. Put your config where the launcher looks for it, and author the Google token
   once (same as before):

   ```powershell
   mkdir "%APPDATA%\oracle" 2>$null
   copy deploy\oracle.windows.toml "%APPDATA%\oracle\oracle.toml"
   .\target\release\oracle-core.exe auth --config "%APPDATA%\oracle\oracle.toml" ^
       --credentials "E:\oracle-models\credentials.json" --account default
   ```

3. In `%APPDATA%\oracle\oracle.toml`, look at the `[supervise]` block:
   - Point `llm_program` / `llm_args` at your llama-server and model, then set
     `autostart_llm = true` — now the LLM server comes up with everything else.
     (Leave it `false` if you'd rather start llama-server yourself.)
   - `autostart_actd = true` and `open_window = true` are already set.
   - `browser = "edge"` gives the chromeless window; `"default"` opens a normal tab.

## Running it

Double-click **`deploy\Oracle.vbs`** (or make a desktop shortcut to it, or drop
that shortcut in `shell:startup` to have the Oracle ready at login). That's it —
no terminal, and if it's already running it just brings the window forward.

To stop everything, run **`deploy\Stop-Oracle.bat`** (adjust the LLM exe name in
it if yours isn't `llama-server.exe`).

Logs, if you need them: `%APPDATA%\oracle\run\oracle.log` (core),
`llm.log`, and `actd.log` in the same folder.

## Voice replies

Voice output is now **on by default** and no longer tied to whether the mic is
listening, so typed questions get spoken answers too. There's a `🔊 Voice reply`
button in the HUD to mute/unmute, and the `◼ Interrupt` button stops speech
mid-sentence. Because the window is real Edge/Chrome, speech recognition works as
well — click `🎙 Voice` to talk.

## What about the Tauri app?

It still exists under `oracle-app/`, but it's now **optional**. Everything it did
— hosting the HUD, supervising the daemons, a summon hotkey — either lives in
`oracle-core` now or is a small follow-up (a global summon hotkey + wake word,
which belong in core so they work while the window is closed). For the "just run
it" experience, use `Oracle.vbs`; you don't need to build Tauri at all.
