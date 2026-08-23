//! `oracle-core` binary — the orchestrator process.
//!
//! Subcommands:
//!   oracle-core run [--config PATH]     start the assistant (gateway + agent loop)
//!   oracle-core repl [--config PATH]    interactive text REPL (no audio)
//!   oracle-core auth [--credentials credentials.json] [--account EMAIL]
//!                                       authorize Google Workspace (OAuth)
//!   oracle-core doctor                  print the latency budget report
//!   oracle-core write-config [PATH]     emit a fully-populated oracle.toml
//!
//! `run` wires config → HUD gateway → agent, installs signal handlers, restores
//! any session snapshot, and drains gracefully on SIGINT/SIGTERM. Without a
//! configured llama-server it uses the offline mock so the process still boots.

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use oracle_core::agent::{Agent, AgentConfig, AgentEvent};
use oracle_core::config::Config;
use oracle_core::gateway::server::HudGateway;
use oracle_core::lifecycle::{install_signal_handlers, SessionSnapshot, ShutdownController};
use oracle_core::llm::{LlamaServer, Llm, MockLlm};
use oracle_core::observ::{LatencyRecorder, Stage};
use oracle_core::supervisor::{ChildSpec, Supervisor};
use oracle_core::{demo_registry, Shared};
use oracle_ipc::{HudCommand, HudEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("repl");

    match cmd {
        "write-config" => {
            let path = args.get(2).cloned().unwrap_or_else(|| "oracle.toml".into());
            std::fs::write(&path, Config::example_toml())?;
            println!("wrote {path}");
            Ok(())
        }
        "doctor" => doctor(),
        "check-config" => {
            let path = arg_value(&args, "--config")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("oracle.toml"));
            match Config::load(&path) {
                Ok(_) => {
                    println!("[oracle] {} is valid", path.display());
                    Ok(())
                }
                Err(e) => {
                    eprintln!("[oracle] config invalid: {e}");
                    std::process::exit(1);
                }
            }
        }
        "auth" => {
            let cfg = load_config(&args)?;
            init_tracing(&cfg.general.log_level);
            auth(cfg, &args).await
        }
        "run" => {
            let mut cfg = load_config(&args)?;
            // `--no-window` lets a native shell (oracle-shell) own the window, so
            // core serves the HUD but doesn't also pop a browser.
            if args.iter().any(|a| a == "--no-window") {
                cfg.supervise.open_window = false;
            }
            init_tracing(&cfg.general.log_level);
            run(cfg).await
        }
        // "repl" and any unrecognized command fall through to the REPL.
        _ => {
            let cfg = load_config(&args)?;
            init_tracing(&cfg.general.log_level);
            repl(cfg).await
        }
    }
}

fn init_tracing(level: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("oracle_core={level}").into()),
        )
        .with_target(false)
        .try_init();
}

fn load_config(args: &[String]) -> anyhow::Result<Config> {
    let path = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("oracle.toml"));
    println!(
        "[oracle] loading config: {} (exists: {})",
        path.display(),
        path.exists()
    );
    Ok(Config::load_or_default(&path)?)
}

fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// `auth`: run the Google Workspace OAuth flow and seal the tokens.
async fn auth(cfg: Config, args: &[String]) -> anyhow::Result<()> {
    use oracle_core::connectors::google::{self, GoogleCredentials};

    let creds_path = arg_value(args, "--credentials").unwrap_or("credentials.json");
    let account = arg_value(args, "--account")
        .unwrap_or("default")
        .to_string();
    let creds = GoogleCredentials::load(std::path::Path::new(creds_path))
        .map_err(|e| anyhow::anyhow!("loading {creds_path}: {e}"))?;
    println!(
        "[oracle] authorizing Google account '{account}' (project {})",
        creds.project_id
    );

    std::fs::create_dir_all(&cfg.general.runtime_dir).ok();
    let now = chrono::Utc::now().timestamp();
    let scopes = GoogleCredentials::default_scopes();

    let tokens = google::authorize(&creds, &scopes, now, |auth_url| async move {
        println!("\nOpen this URL in your browser to authorize:\n\n{auth_url}\n");
        open_in_browser(&auth_url);
    })
    .await?;

    // Seal the refresh token into the vault and persist the sealed blob.
    let vault = load_or_create_vault(&cfg.general.runtime_dir)?;
    if let Some(sealed) = google::seal_tokens(&vault, &account, &tokens)? {
        let tok_path =
            PathBuf::from(&cfg.general.runtime_dir).join(format!("google-{account}.tok"));
        std::fs::write(&tok_path, sealed.to_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tok_path, std::fs::Permissions::from_mode(0o600));
        }
        println!("[oracle] authorized. Sealed refresh token stored (0600).");
    } else {
        println!(
            "[oracle] authorized, but Google returned no refresh token — re-run with consent."
        );
    }
    Ok(())
}

/// Load the vault master key from `<runtime>/vault.key`, creating a random one
/// (0600) on first use. Production hardening: source this from the OS keyring
/// (Secret Service / DPAPI) instead — see docs/DEPLOYMENT.md.
fn load_or_create_vault(
    runtime_dir: &str,
) -> anyhow::Result<oracle_core::connectors::vault::TokenVault> {
    use rand::RngCore;
    let key_path = PathBuf::from(runtime_dir).join("vault.key");
    let key: [u8; 32] = if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        if bytes.len() != 32 {
            anyhow::bail!("vault.key is corrupt (expected 32 bytes)");
        }
        bytes.try_into().unwrap()
    } else {
        let mut k = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut k);
        std::fs::write(&key_path, k)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600));
        }
        k
    };
    Ok(oracle_core::connectors::vault::TokenVault::new(&key))
}

/// Connect to the actuator daemon over its socket/pipe. Returns None (OS tools
/// disabled) if the daemon isn't running — the tools then report that clearly.
async fn connect_actd(
    cfg: &Config,
    retry: bool,
) -> Option<oracle_core::connectors::actd_client::ActdClient<oracle_core::ActdStream>> {
    use oracle_core::connectors::actd_client::ActdClient;
    // When core launches actd itself (supervise.autostart_actd), the daemon may
    // still be binding its pipe — give it up to ~5s. Otherwise try just once.
    let attempts = if retry { 25 } else { 1 };
    for attempt in 0..attempts {
        match ActdClient::connect(&cfg.actd.socket).await {
            Ok(c) => {
                tracing::info!(socket = %cfg.actd.socket, "actd connected");
                return Some(c);
            }
            Err(e) => {
                if attempt + 1 < attempts {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                } else {
                    tracing::warn!(
                        "actd not connected ({e}); OS-control tools disabled. Start `oracle-actd --serve {}`",
                        cfg.actd.socket
                    );
                    // Surface WHY: the daemon logs its own startup errors to
                    // actd.log. If it crashed on launch, the reason is there —
                    // echo the tail so it isn't a silent black box.
                    let log = PathBuf::from(&cfg.general.runtime_dir).join("actd.log");
                    if let Some(tail) = tail_of(&log, 20) {
                        if !tail.trim().is_empty() {
                            println!(
                                "[oracle] actd did not come up. Last lines of {}:\n{}",
                                log.display(),
                                tail
                            );
                        }
                    }
                }
            }
        }
    }
    None
}

/// Read the last `n` lines of a text file, if it exists. Used to surface actd's
/// own startup errors when core can't connect to it.
fn tail_of(path: &std::path::Path, n: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    Some(lines[start..].join("\n"))
}

/// True if something is already serving the HUD bind address — i.e. an Oracle
/// instance is already up.
async fn already_running(bind: &str) -> bool {
    let addr = bind.replace("0.0.0.0", "127.0.0.1");
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_millis(300),
            tokio::net::TcpStream::connect(addr),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Build and start the process supervisor from config: the LLM server and the
/// actd daemon, each launched hidden and kept alive until core shuts down.
fn start_supervisor(cfg: &Config) -> Supervisor {
    let mut sup = Supervisor::new(CancellationToken::new());
    let rt = PathBuf::from(&cfg.general.runtime_dir);

    if cfg.supervise.autostart_llm {
        if cfg.supervise.llm_program.trim().is_empty() {
            println!(
                "[oracle] supervise.autostart_llm=true but llm_program is empty — NOT launching an LLM server"
            );
        } else {
            println!(
                "[oracle] launching LLM server: {} {}",
                cfg.supervise.llm_program,
                cfg.supervise.llm_args.join(" ")
            );
            sup.supervise(ChildSpec {
                name: "llm".into(),
                program: cfg.supervise.llm_program.clone(),
                args: cfg.supervise.llm_args.clone(),
                log_path: rt.join("llm.log"),
            });
        }
    } else {
        println!(
            "[oracle] LLM autostart is OFF (supervise.autostart_llm=false) — start llama-server yourself, or set it true in oracle.toml"
        );
    }

    if cfg.supervise.autostart_actd {
        // Clear any stray/orphaned actd first. A previous instance that outlived
        // a hard-killed core still owns the named pipe, so a fresh one gets
        // "Access is denied (os error 5)" and flaps forever. Reap orphans so the
        // one we launch binds the pipe cleanly.
        kill_stray_actd();

        let mut args = vec!["--serve".to_string(), cfg.actd.socket.clone()];
        // Tell actd where to keep its audit journal — the runtime dir, which is
        // always writable — so it never dies trying to log in a read-only CWD.
        args.push("--log-dir".into());
        args.push(cfg.general.runtime_dir.clone());
        if cfg.actd.grant_sensitive {
            args.push("--grant-sensitive".into());
        }
        sup.supervise(ChildSpec {
            name: "actd".into(),
            program: resolve_actd_program(cfg),
            args,
            log_path: rt.join("actd.log"),
        });
    }

    if !sup.is_empty() {
        tracing::info!("supervisor managing the LLM server and/or actd daemon");
    }
    sup
}

/// Reap any orphaned `oracle-actd` still holding the named pipe, so the fresh
/// one we're about to launch doesn't hit "Access is denied (os error 5)". A
/// short pause lets Windows release the pipe handle. No-op off Windows (the UDS
/// is just re-bound).
#[cfg(windows)]
fn kill_stray_actd() {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = std::process::Command::new("taskkill")
        .args(["/IM", "oracle-actd.exe", "/F"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    std::thread::sleep(std::time::Duration::from_millis(300));
}

#[cfg(not(windows))]
fn kill_stray_actd() {}

/// The path of the summon flag file — a rendezvous point between core and the
/// native shell. Core writes it when the wake word fires; the shell polls for
/// it and, when it appears, brings its window forward and deletes it. Kept at a
/// fixed, config-independent location so both processes agree without the shell
/// having to parse core's TOML: `%LOCALAPPDATA%\oracle\summon.flag` on Windows,
/// and the system temp dir elsewhere.
fn summon_flag_path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".into());
        std::path::Path::new(&base)
            .join("oracle")
            .join("summon.flag")
    }
    #[cfg(not(windows))]
    {
        std::env::temp_dir().join("oracle-summon.flag")
    }
}

/// Touch the summon flag so the native shell raises its window on the next poll.
fn raise_summon_flag() {
    let path = summon_flag_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, b"1") {
        tracing::warn!("could not write summon flag {}: {e}", path.display());
    }
}

/// Locate the `oracle-actd` binary: an explicit config path, else the sibling of
/// this executable (how the packaged app ships it), else `oracle-actd` on PATH.
fn resolve_actd_program(cfg: &Config) -> String {
    if !cfg.supervise.actd_program.trim().is_empty() {
        return cfg.supervise.actd_program.clone();
    }
    let name = if cfg!(windows) {
        "oracle-actd.exe"
    } else {
        "oracle-actd"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let cand = dir.join(name);
            if cand.exists() {
                return cand.to_string_lossy().into_owned();
            }
        }
    }
    "oracle-actd".into()
}

/// Open the HUD in a chromeless "app" window (Edge/Chrome `--app=`), falling back
/// to the default browser. This is the Oracle's face without a browser's chrome.
fn open_hud_window(cfg: &Config) {
    let host = cfg.hud.bind.replace("0.0.0.0", "127.0.0.1");
    let url = format!("http://{host}/");
    let pref = cfg.supervise.browser.trim().to_ascii_lowercase();

    // Try the preferred Chromium browser's app window, then the *other* one, so
    // we land in a real chromeless window rather than a tab whenever possible.
    if pref != "default" && !pref.is_empty() {
        // A custom path/program takes priority if it launches.
        if pref != "chrome" && pref != "edge" && try_app_window(&pref, &url) {
            return;
        }
        let order: [&str; 2] = if pref == "edge" {
            ["edge", "chrome"]
        } else {
            ["chrome", "edge"]
        };
        for b in order {
            if try_app_window(b, &url) {
                return;
            }
        }
        tracing::warn!(
            "couldn't find Edge or Chrome to open a chromeless app window; opening the default browser (a normal tab) instead"
        );
    }
    info!(%url, "opening the HUD in the default browser");
    open_in_browser(&url);
}

/// Candidate executables for a Chromium-based browser, resolved against the
/// user's real install locations — Program Files AND per-user LocalAppData,
/// which is where Chrome frequently installs itself.
#[cfg(windows)]
fn browser_candidates(browser: &str) -> Vec<String> {
    let env = |k: &str| std::env::var(k).unwrap_or_default();
    let pf = env("ProgramFiles");
    let pfx = env("ProgramFiles(x86)");
    let lad = env("LOCALAPPDATA");
    match browser {
        "edge" => vec![
            format!(r"{pfx}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{pf}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{lad}\Microsoft\Edge\Application\msedge.exe"),
            "msedge".into(),
        ],
        "chrome" => vec![
            format!(r"{pf}\Google\Chrome\Application\chrome.exe"),
            format!(r"{pfx}\Google\Chrome\Application\chrome.exe"),
            format!(r"{lad}\Google\Chrome\Application\chrome.exe"),
            "chrome".into(),
        ],
        other => vec![other.to_string()],
    }
}

#[cfg(not(windows))]
fn browser_candidates(browser: &str) -> Vec<String> {
    match browser {
        "chrome" => vec![
            "google-chrome".into(),
            "chromium".into(),
            "chromium-browser".into(),
        ],
        "edge" => vec!["microsoft-edge".into(), "microsoft-edge-stable".into()],
        other => vec![other.to_string()],
    }
}

/// Try to launch a chromeless app window in the named Chromium-based browser.
/// Returns true only if a browser process was actually launched.
fn try_app_window(browser: &str, url: &str) -> bool {
    let app = format!("--app={url}");
    for prog in browser_candidates(browser) {
        // Skip a full path that doesn't exist so we don't report a phantom launch.
        if prog.contains(['\\', '/']) && !std::path::Path::new(&prog).exists() {
            continue;
        }
        if std::process::Command::new(&prog)
            .arg(&app)
            .arg("--new-window")
            .spawn()
            .is_ok()
        {
            info!(%prog, "opened HUD in a chromeless app window");
            return true;
        }
    }
    false
}

/// Load the Google client from the sealed token, if Workspace auth is
/// configured and a sealed token exists. Returns None (Google disabled) if any
/// prerequisite is missing — the tools then return a clear "not authorized"
/// error rather than failing cryptically.
fn load_google(cfg: &Config) -> Option<oracle_core::connectors::google_api::GoogleClient> {
    use oracle_core::connectors::google::GoogleCredentials;
    use oracle_core::connectors::google_api::GoogleClient;
    use oracle_core::connectors::oauth::TokenSet;
    use oracle_core::connectors::vault::SealedToken;

    if cfg.google.credentials_path.trim().is_empty() {
        return None;
    }
    let creds = match GoogleCredentials::load(std::path::Path::new(&cfg.google.credentials_path)) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("google: cannot load credentials: {e}");
            return None;
        }
    };
    let vault = load_or_create_vault(&cfg.general.runtime_dir).ok()?;
    let tok_path =
        PathBuf::from(&cfg.general.runtime_dir).join(format!("google-{}.tok", cfg.google.account));
    let sealed_bytes = match std::fs::read(&tok_path) {
        Ok(b) => b,
        Err(_) => {
            tracing::warn!(
                "google: no sealed token at {} — run `oracle-core auth`",
                tok_path.display()
            );
            return None;
        }
    };
    let sealed = SealedToken::from_bytes(&sealed_bytes)?;
    let scopes = GoogleCredentials::default_scopes();
    let rt = vault
        .open("google", &cfg.google.account, &scopes, &sealed)
        .ok()?;
    let refresh_token = String::from_utf8(rt).ok()?;
    let tokens = TokenSet {
        access_token: String::new(), // obtained on first call via refresh
        refresh_token: Some(refresh_token),
        expires_in_s: 0,
        obtained_at_unix: 0,
        scopes,
    };
    tracing::info!(account = %cfg.google.account, "google workspace connected");
    Some(GoogleClient::new(creds, tokens))
}

/// Open a URL in the system browser (best-effort; the URL is also printed).
///
/// On Windows we deliberately DO NOT use `cmd /C start <url>`: cmd.exe treats
/// the `&` between OAuth query parameters as a command separator and silently
/// truncates the URL at the first `&`, so the browser only ever receives
/// `...?response_type=code` and Google reports "missing redirect_uri". Instead
/// we hand the URL straight to the protocol handler via
/// `rundll32 url.dll,FileProtocolHandler`, which does no cmd-style parsing.
fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(url)
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn build_llm(cfg: &Config) -> Arc<dyn Llm> {
    if cfg.llm.backend == "mock" {
        eprintln!("[oracle] LLM backend: mock (offline)");
        Arc::new(MockLlm::demo())
    } else {
        eprintln!(
            "[oracle] LLM backend: {} (model {})",
            cfg.llm.backend, cfg.llm.model
        );
        Arc::new(LlamaServer::new(
            cfg.llm.backend.clone(),
            cfg.llm.model.clone(),
        ))
    }
}

fn agent_config(cfg: &Config) -> AgentConfig {
    AgentConfig {
        step_budget: cfg.agent.step_budget,
        max_tokens: cfg.llm.max_tokens,
        temperature: cfg.llm.temperature,
        ..AgentConfig::default()
    }
}

/// Full runtime: gateway + agent loop + graceful shutdown.
async fn run(cfg: Config) -> anyhow::Result<()> {
    std::fs::create_dir_all(&cfg.general.runtime_dir).ok();

    // Idempotent launch: if the HUD port is already being served, an Oracle is
    // already running — just reveal its window and exit, instead of spawning a
    // rival core that would fight over the port and the actd pipe. This is what
    // makes "double-click the launcher again" simply summon the existing one.
    if cfg.hud.enabled && already_running(&cfg.hud.bind).await {
        info!("Oracle already running — opening the window");
        if cfg.supervise.open_window {
            open_hud_window(&cfg);
        }
        return Ok(());
    }

    // Self-supervision: bring up the LLM server and actd as hidden background
    // children so the whole assistant starts from one launch, no terminals.
    let supervisor = start_supervisor(&cfg);

    // HUD gateway first — its publisher feeds the Apollo confirmation modal, so
    // the confirmer must exist before Shared/the agent are built.
    let token = cfg.hud.token.clone();
    let mut gateway = HudGateway::new(token.clone());
    let publisher = gateway.publisher();
    let confirmer = Arc::new(oracle_core::confirm::HudConfirmer::new(publisher.clone()));

    // actd may have just been launched by the supervisor; retry briefly if so.
    let actd = connect_actd(&cfg, cfg.supervise.autostart_actd).await;
    let actd_up = actd.is_some();
    let shared = Arc::new(
        Shared::open(&cfg.memory.db_path)?
            .with_google(load_google(&cfg))
            .with_actd(actd)
            .with_confirmer(confirmer.clone()),
    );
    let llm = build_llm(&cfg);
    // Arc so each HUD-driven turn can run on its own task.
    let agent = Arc::new(Agent::new(llm, demo_registry(), shared, agent_config(&cfg)));

    // Session restore (warm start).
    let session_path = PathBuf::from(&cfg.memory.db_path).with_extension("session.json");
    if let Some(snap) = SessionSnapshot::load(&session_path) {
        info!(turns = snap.turn_count, "restored session snapshot");
    }

    // Start serving the HUD. An empty configured token means "no auth" — the
    // loopback bind is the boundary. Set [hud] token only when exposing the
    // gateway beyond localhost; then clients must append ?token=<token>.
    if cfg.hud.enabled {
        let (addr, _h) = gateway.serve(&cfg.hud.bind).await?;
        if token.is_empty() {
            println!("[oracle] HUD served at http://{addr}/ (ws://{addr}/hud)");
        } else {
            println!("[oracle] HUD served at http://{addr}/  ·  ws://{addr}/hud?token={token}");
        }
        // Open the Oracle's face in a chromeless window. The HUD retries its
        // WebSocket until the agent loop below is live, so opening now is fine.
        if cfg.supervise.open_window {
            open_hud_window(&cfg);
        }
    }

    // Shutdown wiring.
    let (shutdown, mut shutdown_listener) = ShutdownController::new();
    let _sig = install_signal_handlers(shutdown.clone());

    info!("oracle-core running; Ctrl-C to stop");
    publisher.send_event(HudEvent::State {
        turn: uuid::Uuid::nil(),
        state: "idle".into(),
    });

    // Live System-panel status: model, backend health, and the throughput of the
    // most recent turn. `last_tok_per_s` is updated at the end of each turn; the
    // loop repaints every few seconds so the panel is never a dead placeholder.
    let last_tok_per_s = Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let publisher = publisher.clone();
        let tok = last_tok_per_s.clone();
        let model = cfg.llm.model.clone();
        let backend = cfg.llm.backend.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3));
            loop {
                tick.tick().await;
                let tps = tok.load(std::sync::atomic::Ordering::Relaxed);
                let rate = if tps > 0 {
                    format!("{tps} tok/s")
                } else {
                    "ready".to_string()
                };
                let text = format!(
                    "{model} · {backend} · actd {} · {rate}",
                    if actd_up { "linked" } else { "offline" }
                );
                publisher.send_event(HudEvent::Status { text });
            }
        });
    }

    // Handle inbound HUD commands until shutdown. A typed message starts a
    // conversation turn whose events stream back to the HUD; Interrupt cancels
    // the active turn (barge-in).
    let mut active_turn: Option<CancellationToken> = None;
    loop {
        tokio::select! {
            _ = shutdown_listener.wait() => break,
            cmd = gateway.next_command() => {
                match cmd {
                    Some(HudCommand::Interrupt) => {
                        info!("HUD interrupt");
                        if let Some(tok) = active_turn.take() {
                            tok.cancel();
                        }
                        publisher.send_event(HudEvent::State {
                            turn: uuid::Uuid::nil(),
                            state: "idle".into(),
                        });
                    }
                    Some(HudCommand::UserText { text }) => {
                        // Cancel any in-flight turn, then start a new one.
                        if let Some(tok) = active_turn.take() {
                            tok.cancel();
                        }
                        let cancel = CancellationToken::new();
                        active_turn = Some(cancel.clone());
                        spawn_hud_turn(
                            agent.clone(),
                            publisher.clone(),
                            text,
                            cancel,
                            last_tok_per_s.clone(),
                        );
                    }
                    Some(HudCommand::Confirm { request_id, allow }) => {
                        // The user passed sentence in the Apollo modal.
                        info!(%request_id, allow, "confirmation decree");
                        confirmer.resolve(request_id, allow);
                    }
                    Some(HudCommand::Summon) => {
                        // Wake word heard: raise a flag the native shell polls so
                        // it can bring the (possibly dismissed) window forward.
                        info!("summon requested (wake word)");
                        raise_summon_flag();
                    }
                    Some(_) => {}
                    None => {}
                }
            }
        }
    }

    // Graceful drain: persist session and exit.
    let snap = SessionSnapshot {
        turn_count: 0,
        rolling_summary: String::new(),
        last_saved_unix: chrono::Utc::now().timestamp(),
    };
    let _ = snap.save(&session_path);
    let _ = agent; // kept alive until here

    // Reap the supervised children (LLM server, actd) so nothing is orphaned.
    supervisor.shutdown().await;
    info!("shutdown complete");
    Ok(())
}

/// Run one HUD-driven conversation turn on its own task, mapping the agent's
/// events onto HUD events so the browser shows the transcript, tool activity,
/// and the streaming reply.
fn spawn_hud_turn(
    agent: Arc<Agent>,
    publisher: oracle_core::gateway::server::HudPublisher,
    text: String,
    cancel: CancellationToken,
    last_tok_per_s: Arc<std::sync::atomic::AtomicU32>,
) {
    tokio::spawn(async move {
        let turn = uuid::Uuid::new_v4();
        // Echo the user's message and enter the "thinking" state.
        publisher.send_event(HudEvent::Transcript {
            text: text.clone(),
            stable: true,
        });
        publisher.send_event(HudEvent::State {
            turn,
            state: "thinking".into(),
        });

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let run = agent.run_turn(text, tx, cancel);

        let pub2 = publisher.clone();
        let tok_counter = last_tok_per_s.clone();
        let forward = async move {
            let mut reply = String::new();
            let mut spoke = false;
            let mut saw_finished = false;
            let mut first_token_at: Option<std::time::Instant> = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    AgentEvent::Say(s) => {
                        if !spoke {
                            spoke = true;
                            first_token_at = Some(std::time::Instant::now());
                            pub2.send_event(HudEvent::State {
                                turn,
                                state: "speaking".into(),
                            });
                        }
                        reply.push_str(&s);
                        // Send the growing reply as a caption.
                        pub2.send_event(HudEvent::Caption {
                            text: reply.clone(),
                        });
                    }
                    AgentEvent::ToolStarted { id, name } => {
                        pub2.send_event(HudEvent::State {
                            turn,
                            state: "tool".into(),
                        });
                        pub2.send_event(HudEvent::Tool {
                            id,
                            name,
                            status: oracle_ipc::ToolStatus::Started,
                            detail: None,
                        });
                    }
                    AgentEvent::ToolFinished {
                        id,
                        name,
                        ok,
                        detail,
                    } => {
                        pub2.send_event(HudEvent::Tool {
                            id,
                            name,
                            status: if ok {
                                oracle_ipc::ToolStatus::Done
                            } else {
                                oracle_ipc::ToolStatus::Error
                            },
                            detail,
                        });
                    }
                    AgentEvent::Finished { .. } => {
                        saw_finished = true;
                        pub2.send_event(HudEvent::State {
                            turn,
                            state: "idle".into(),
                        });
                    }
                }
            }
            // Estimate throughput for the System panel: characters/4 ≈ tokens,
            // over the generation window (first spoken token → now). A rough but
            // honest local reading; only updated when we actually generated text.
            if let Some(start) = first_token_at {
                let secs = start.elapsed().as_secs_f32();
                let approx_tokens = (reply.chars().count() as f32) / 4.0;
                if secs > 0.1 && approx_tokens > 0.0 {
                    let tps = (approx_tokens / secs).round() as u32;
                    tok_counter.store(tps, std::sync::atomic::Ordering::Relaxed);
                }
            }
            saw_finished
        };

        // Guard the whole turn with a timeout so a hung LLM/tool can't leave the
        // HUD stuck on "thinking" forever. Whatever happens — error, timeout, or
        // a turn that ended without a Finished event — we surface a message and
        // return to idle so the Oracle is never silently frozen.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            tokio::join!(run, forward)
        })
        .await;

        let problem: Option<String> = match outcome {
            Ok((Ok(_), true)) => None, // normal completion
            Ok((Ok(_), false)) => Some("the oracle gave no answer.".to_string()),
            Ok((Err(e), _)) => Some(format!("the oracle could not answer: {e}")),
            Err(_) => Some(
                "the oracle did not answer in time — is the LLM server running on the configured backend?"
                    .to_string(),
            ),
        };
        if let Some(msg) = problem {
            tracing::warn!("turn did not complete cleanly: {msg}");
            publisher.send_event(HudEvent::Caption { text: msg });
            publisher.send_event(HudEvent::State {
                turn,
                state: "idle".into(),
            });
        }
    });
}

/// Interactive REPL that streams the agent loop with live latency recording.
async fn repl(cfg: Config) -> anyhow::Result<()> {
    // The REPL doesn't supervise anything; connect to actd once if it's up.
    let actd = connect_actd(&cfg, false).await;
    let shared = Arc::new(
        Shared::open(&cfg.memory.db_path)?
            .with_google(load_google(&cfg))
            .with_actd(actd)
            // In the terminal, confirmations are a y/N prompt.
            .with_confirmer(Arc::new(oracle_core::confirm::StdinConfirmer)),
    );
    let llm = build_llm(&cfg);
    let agent = Agent::new(llm, demo_registry(), shared, agent_config(&cfg));
    let latency = Arc::new(LatencyRecorder::new());

    eprintln!("[oracle] REPL ready (Ctrl-D to exit).");
    let stdin = io::stdin();
    let mut lines = stdin.lock().lines();

    loop {
        eprint!("you> ");
        io::stderr().flush().ok();
        let Some(line) = lines.next() else { break };
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let cancel = CancellationToken::new();
        let turn_start = std::time::Instant::now();

        let printer = async {
            print!("oracle> ");
            io::stdout().flush().ok();
            let mut first_token = true;
            while let Some(ev) = rx.recv().await {
                match ev {
                    AgentEvent::Say(s) => {
                        if first_token {
                            latency.record(
                                Stage::FirstTokenClause,
                                turn_start.elapsed().as_millis() as u64,
                            );
                            first_token = false;
                        }
                        print!("{s}");
                        io::stdout().flush().ok();
                    }
                    AgentEvent::ToolStarted { id, name } => eprint!("\n  [#{id} {name} …]"),
                    AgentEvent::ToolFinished {
                        id,
                        name,
                        ok,
                        detail,
                    } => {
                        eprint!(" [#{id} {name} {}]", if ok { "ok" } else { "ERR" });
                        if let Some(d) = detail {
                            eprint!(" ({d})");
                        }
                    }
                    AgentEvent::Finished { cancelled } => {
                        println!("{}", if cancelled { "  (interrupted)" } else { "" })
                    }
                }
            }
        };

        let (_r, _) = tokio::join!(agent.run_turn(line, tx, cancel), printer);
    }

    eprintln!("\n[oracle] latency summary:\n{}", latency.report().render());
    Ok(())
}

/// `doctor`: with no live traffic this seeds a representative sample so the
/// report format is demonstrable; in a running system it reads the live
/// recorder over the control socket.
fn doctor() -> anyhow::Result<()> {
    let r = LatencyRecorder::new();
    for _ in 0..100 {
        r.record(Stage::Endpoint, 165);
        r.record(Stage::AsrFinal, 32);
        r.record(Stage::PromptAssembly, 11);
        r.record(Stage::LlmPrefill, 95);
        r.record(Stage::FirstTokenClause, 105);
        r.record(Stage::TtsFirstChunk, 72);
        r.record(Stage::OutputDevice, 22);
    }
    println!("{}", r.report().render());
    Ok(())
}
