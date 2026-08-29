# Operations Runbook

Day-two operations: what to check, what breaks, and how to fix it.

## Health checks

```bash
oracle-core doctor                          # latency budget, names the worst stage
systemctl --user status oracle-{actd,core,audio}
journalctl --user -u oracle-core -f         # live logs
tail -f "$XDG_RUNTIME_DIR/oracle/actd-audit.jsonl"   # privileged-action audit
```

`doctor` is the first thing to run when "it feels slow." It reports p50/p95/p99
per pipeline stage against the design budget and names the stage that's over.

## Symptom → cause → fix

### "It doesn't respond to my voice"
1. Is `oracle-audio` running? `systemctl --user status oracle-audio`.
2. Does it see the mic? Run `oracle-audio` in a terminal; the capture backend
   and sample count print at startup. `backend=null` means ALSA wasn't compiled
   or the device failed to open — rebuild with `--alsa` and check `arecord -l`.
3. Barge-in cutting you off / echoing? AEC isn't converging — check that the TTS
   output device is the reference tap (see `docs/DEPLOYMENT.md` §4).

### "It hears me but never answers"
- `llm.backend` points at a llama-server that's down. `curl $BACKEND/health`.
  Falls back to nothing if unreachable; core logs the connection error.
- Check `oracle-core doctor` — if `llm_prefill` or `first_token_clause` is
  massively over budget, the model is too big for the GPU (swapping) or context
  is exhausted.

### "It answers but won't do actions" (lights, email, files)
- `oracle-actd` down or socket missing: `ls -l $XDG_RUNTIME_DIR/oracle/actd.sock`
  (should be `srw-------`). Restart actd.
- Action needs confirmation and none was given: this is by design for T2/
  irreversible ops. Say "yes" or click confirm.
- In `LOCKDOWN`: you (or a panic gesture) locked it. Say "unlock" or restart
  actd.
- Capability not granted: with `grant_sensitive=false`, sensitive ops prompt
  every time. Set it `true` in config for a standing session grant (revocable).

### "Google stopped working"
- Token expired and refresh failed (`invalid_grant`, e.g. password change or
  revoked consent). Core marks the account degraded and serves reads from the
  local index. Re-auth: trigger the OAuth flow again (browser consent).
- Never edit the vault by hand; delete the sealed token and re-authorize.

### "Home Assistant / MQTT entities are stale"
- The WS/MQTT connection dropped; both reconnect with backoff. Check core logs
  for `HA read pump ended` or `mqtt eventloop error`. The mirror keeps its last
  known state until reconnect — reads won't error, they'll just be stale.

### "The HUD is black / won't connect"
- The token is wrong or missing. Core prints the full URL with the token at
  startup: `ws://127.0.0.1:8770/hud?token=...`. Copy it exactly.
- Frame rate tanking? The HUD auto-degrades (resolution → particle count →
  effects). If it's pinned low, the GPU is shared with the LLM — expected during
  heavy generation.

## Restart & recovery

All three units are `Restart=always`/`on-failure`. On a core restart:
- SQLite (WAL) and the knowledge graph are durable — no data loss.
- The session snapshot (`*.session.json`) restores the rolling summary and turn
  count for a warm start.
- The LLM KV cache is rebuilt from the journal on the next turn (one prefill).

A crash-looping unit: `journalctl --user -u <unit> -n 100` for the panic, then
`systemctl --user reset-failed <unit>`.

## Backups

Everything durable is one SQLite file (`memory.db_path`) plus the session
snapshot beside it. Back up both:

```bash
sqlite3 "$DB" ".backup '/backup/oracle-$(date +%F).db'"
cp "${DB%.db}.session.json" /backup/
```

The OS keyring holds the vault master key — back it up per your keyring's own
procedure (it is not in the SQLite file).

## Upgrades

Models and ONNX artifacts are content-addressed; config pins the hash. To swap a
model: place the new file, update the hash in config, restart core. Rollback is
restoring the previous hash. Binary upgrades: replace the binary, restart the
unit — state is forward-compatible within a minor version.

## Rotating the HUD token

Set a new `hud.token` (or clear it to auto-generate) and restart core. Existing
HUD tabs will fail to reconnect and must reload with the new URL.
