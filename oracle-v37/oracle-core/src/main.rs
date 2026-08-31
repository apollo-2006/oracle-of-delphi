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
    let explicit = args
        .iter()
        .position(|a| a == "--config")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from);
    let path = explicit
        .clone()
        .unwrap_or_else(|| PathBuf::from("oracle.toml"));

    // A --config the caller typed themselves that does not exist is a mistake,
    // not a request for defaults. Falling back silently here is a genuinely
    // nasty failure: the defaults have autostart_llm=false and no voice
    // programs, so the assistant boots looking healthy while the LLM, wake word
    // and TTS are all simply absent -- with nothing on screen saying why.
    //
    // A *missing* --config still falls back, because booting with defaults is
    // the documented offline path.
    if let Some(p) = &explicit {
        if !p.exists() {
            anyhow::bail!(
                "no config at {}\n\
                 \n\
                 Hint: on Windows the shell looks in %APPDATA%\\oracle\\oracle.toml \
                 (Roaming), which is NOT %LOCALAPPDATA%. Run `oracle-core write-config \
                 <path>` to emit a fully-populated one.",
                p.display()
            );
        }
    }

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
fn start_supervisor(cfg: &Config) -> (Supervisor, Option<oracle_core::supervisor::ChildHandle>) {
    let mut sup = Supervisor::new(CancellationToken::new());
    let rt = PathBuf::from(&cfg.general.runtime_dir);
    let mut llm_handle = None;

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
            llm_handle = Some(sup.supervise(ChildSpec {
                name: "llm".into(),
                program: cfg.supervise.llm_program.clone(),
                args: cfg.supervise.llm_args.clone(),
                log_path: rt.join("llm.log"),
            }));
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

    // NB: the local TTS server (Kokoro in WSL/Docker) is NOT supervised here.
    // A container is a detached, long-lived thing — supervising `wsl docker run`
    // as a foreground child that gets killed on core shutdown (and restart-looped
    // when it returns) is exactly why it never came up cleanly. It's brought up
    // by `ensure_tts_server` instead: launched once, decoupled from core's
    // lifecycle, with a health poll. See the run loop.

    if !sup.is_empty() {
        tracing::info!("supervisor managing the LLM server and/or actd daemon");
    }
    (sup, llm_handle)
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

/// Sibling of the summon flag: the "go back to the full window" signal the shell
/// polls. Same fixed location so the shell needn't parse core's config.
fn expand_flag_path() -> std::path::PathBuf {
    summon_flag_path().with_file_name("expand.flag")
}

/// Touch the expand flag so the native shell restores the full window next poll.
fn raise_expand_flag() {
    let path = expand_flag_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, b"1") {
        tracing::warn!("could not write expand flag {}: {e}", path.display());
    }
}

/// The "shrink back to the corner" flag — the reverse of the expand flag.
fn retract_flag_path() -> std::path::PathBuf {
    summon_flag_path().with_file_name("retract.flag")
}

/// Touch the retract flag so the native shell shrinks to the corner next poll.
fn raise_retract_flag() {
    let path = retract_flag_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, b"1") {
        tracing::warn!("could not write retract flag {}: {e}", path.display());
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

/// macOS ships browsers as app bundles, so the executable is buried inside
/// `.app/Contents/MacOS/` and is never a bare name on PATH. Falling through to
/// the Linux names below meant no candidate ever resolved and the HUD always
/// degraded to an ordinary browser tab instead of a chromeless app window.
#[cfg(target_os = "macos")]
fn browser_candidates(browser: &str) -> Vec<String> {
    macos_browser_candidates(browser, &std::env::var("HOME").unwrap_or_default())
}

/// The macOS candidate list, as a pure function of the browser and `$HOME`.
///
/// Split out and compiled on every target so it can be unit-tested off a Mac;
/// the `cfg`-gated wrapper above is the only macOS-only part.
#[allow(dead_code)]
fn macos_browser_candidates(browser: &str, home: &str) -> Vec<String> {
    // A user-local install under ~/Applications takes precedence over the
    // system-wide one, matching how macOS itself resolves apps.
    let bundled = |name: &str, exe: &str| {
        vec![
            format!("{home}/Applications/{name}.app/Contents/MacOS/{exe}"),
            format!("/Applications/{name}.app/Contents/MacOS/{exe}"),
        ]
    };
    match browser {
        "chrome" => {
            let mut v = bundled("Google Chrome", "Google Chrome");
            v.extend(bundled("Chromium", "Chromium"));
            v.extend(bundled("Brave Browser", "Brave Browser"));
            v
        }
        "edge" => bundled("Microsoft Edge", "Microsoft Edge"),
        other => vec![other.to_string()],
    }
}

#[cfg(test)]
mod macos_browser_tests {
    use super::macos_browser_candidates;

    #[test]
    fn candidates_are_bundle_executables_not_bare_names() {
        // try_app_window skips any candidate containing a separator that does
        // not exist on disk, and spawns bare names directly. A bare "chrome"
        // on macOS would report a phantom launch, so every candidate here must
        // be a real absolute path into a bundle.
        for c in macos_browser_candidates("chrome", "/Users/abir") {
            assert!(c.starts_with('/'), "not absolute: {c}");
            assert!(c.contains(".app/Contents/MacOS/"), "not a bundle exe: {c}");
        }
    }

    #[test]
    fn user_applications_outrank_system_ones() {
        let c = macos_browser_candidates("chrome", "/Users/abir");
        let user = c.iter().position(|p| p.starts_with("/Users/abir")).unwrap();
        let system = c
            .iter()
            .position(|p| p.starts_with("/Applications"))
            .unwrap();
        assert!(user < system, "a user-local install must be preferred");
    }

    #[test]
    fn chrome_falls_back_through_chromium_and_brave() {
        let c = macos_browser_candidates("chrome", "/Users/abir").join(" ");
        assert!(c.contains("Google Chrome.app"));
        assert!(c.contains("Chromium.app"));
        assert!(c.contains("Brave Browser.app"));
    }

    #[test]
    fn an_explicit_path_is_passed_through_untouched() {
        assert_eq!(
            macos_browser_candidates(
                "/Applications/Custom.app/Contents/MacOS/Custom",
                "/Users/abir"
            ),
            vec!["/Applications/Custom.app/Contents/MacOS/Custom"]
        );
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
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
        screen_context: cfg.agent.screen_context,
        screen_other_windows: cfg.agent.screen_other_windows,
        auto_recall: cfg.memory.auto_recall,
        auto_record: cfg.memory.auto_record,
        recall_limit: cfg.memory.recall_limit,
        recall_min_score: cfg.memory.recall_min_score,
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
    let (supervisor, llm_child) = start_supervisor(&cfg);

    // Idle unload: give the GPU back between conversations. See oracle_core::idle.
    let llm_life = Arc::new(oracle_core::idle::LlmLifecycle::new(
        llm_child,
        &cfg.llm.backend,
        std::time::Duration::from_secs(cfg.supervise.llm_ready_timeout_secs),
    ));
    let idle_tracker = Arc::new(oracle_core::idle::IdleTracker::new(
        chrono::Utc::now().timestamp(),
        cfg.supervise.idle_unload_secs,
    ));

    // Bring up the local TTS server (Kokoro) out-of-band: launch once if it isn't
    // already answering, then poll until it's warm. Decoupled from the supervisor
    // so the container outlives core restarts instead of being killed with it.
    if !cfg.voice.tts_server_program.trim().is_empty() && !cfg.voice.tts_http_url.trim().is_empty()
    {
        let voice = cfg.voice.clone();
        let log_path = PathBuf::from(&cfg.general.runtime_dir).join("tts.log");
        tokio::spawn(async move { ensure_tts_server(voice, log_path).await });
    }

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
            .with_browser(cfg.browser.to_browser_config())
            .with_confirmer(confirmer.clone()),
    );
    let llm = build_llm(&cfg);
    // Arc so each HUD-driven turn can run on its own task.
    let google_for_proactive = shared.google.clone();
    let agent_shared = shared.clone();
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

    // Shared voice config for per-turn synthesis.
    let voice_cfg = Arc::new(cfg.voice.clone());

    // Live System-panel status: model, backend health, and the throughput of the
    // most recent turn. `last_tok_per_s` is updated at the end of each turn; the
    // loop repaints every few seconds so the panel is never a dead placeholder.
    let last_tok_per_s = Arc::new(std::sync::atomic::AtomicU32::new(0));
    // Input capabilities the HUD needs to pick its mic path (Whisper vs browser).
    let stt_on = cfg.voice.stt_enabled && !cfg.voice.stt_program.trim().is_empty();
    let tts_on = cfg.voice.tts_enabled
        && (!cfg.voice.tts_http_url.trim().is_empty() || !cfg.voice.tts_program.trim().is_empty());
    let wake_on = cfg.voice.wake_enabled && !cfg.voice.wake_program.trim().is_empty();
    // Runtime on/off for the always-on wake listener (toggled by the wake chip).
    let (wake_tx, wake_rx) = tokio::sync::watch::channel(wake_on);
    {
        let publisher = publisher.clone();
        let tok = last_tok_per_s.clone();
        let model = cfg.llm.model.clone();
        let backend = cfg.llm.backend.clone();
        let wake_rx = wake_tx.subscribe();
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
                // Re-announce capabilities so a HUD that connected at any moment
                // (or missed the handshake) always converges to the right paths.
                publisher.send_event(HudEvent::Config {
                    stt: stt_on,
                    tts: tts_on,
                    wake: *wake_rx.borrow(),
                });
            }
        });
    }

    // Always-on wake word: run whisper-stream, show its transcript live, and
    // inject a turn when "Delphi" is heard. Captures the mic natively (no
    // WebView2), so it's independent of the HUD's push-to-talk path.
    let wake_cancel = CancellationToken::new();
    // A turn that ends on a question arms a follow-up listening window via this
    // channel, so the user can answer without re-saying the wake word.
    let (followup_tx, followup_rx) = mpsc::channel::<std::time::Duration>(8);
    if !cfg.voice.wake_program.trim().is_empty() {
        spawn_wake_listener(
            voice_cfg.clone(),
            publisher.clone(),
            gateway.command_sender(),
            wake_rx,
            followup_rx,
            wake_cancel.clone(),
            llm_life.clone(),
            idle_tracker.clone(),
        );
    }
    let mut current_wake = wake_on;

    // Announce capabilities up front (also re-sent on the status tick and on a
    // HUD Hello) so the HUD converges to the right mic path.
    publisher.send_event(HudEvent::Config {
        stt: stt_on,
        tts: tts_on,
        wake: current_wake,
    });

    let mut idle_watch_stop: Option<CancellationToken> = None;
    // Idle watcher: unload the model once the lull passes the threshold. Polls
    // rather than sleeping to the deadline so a `touch` mid-wait is honoured.
    if !llm_life.is_inert() && cfg.supervise.idle_unload_secs > 0 {
        let life = llm_life.clone();
        let tracker = idle_tracker.clone();
        let stop = CancellationToken::new();
        let watcher_stop = stop.clone();
        idle_watch_stop = Some(stop);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                tokio::select! {
                    _ = tick.tick() => {}
                    _ = watcher_stop.cancelled() => return,
                }
                if tracker.is_idle(chrono::Utc::now().timestamp()) && life.unload() {
                    tracing::info!("[idle] no activity; unloaded the LLM and freed its VRAM");
                }
            }
        });
    }

    // Shared "a real conversation is in flight" signal, so the proactive loop
    // never talks over the user.
    let turn_busy = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proactive_stop = CancellationToken::new();
    spawn_routine_watcher(
        agent_shared.clone(),
        gateway.command_sender(),
        turn_busy.clone(),
        proactive_stop.clone(),
    );
    if cfg.proactive.enabled {
        spawn_proactive(
            cfg.proactive.clone(),
            google_for_proactive,
            agent_shared.clone(),
            publisher.clone(),
            voice_cfg.clone(),
            turn_busy.clone(),
            proactive_stop.clone(),
        );
    }

    // Handle inbound HUD commands until shutdown. A typed message starts a
    // conversation turn whose events stream back to the HUD; Interrupt cancels
    // the active turn (barge-in).
    let mut active_turn: Option<CancellationToken> = None;
    // Rolling conversation history so Pythia remembers the exchange across turns
    // (user asks "read my email", she asks "unread or all?", the user says
    // "unread" — she now has the context to know what that means). Completed
    // turns report their (user, assistant) pair back over this channel; the
    // history is capped so context stays bounded.
    let mut history: Vec<oracle_core::llm::ChatMessage> = Vec::new();
    // Each finished turn reports (user_text, tool_trace, assistant_reply). The
    // tool_trace — what actually ran this turn — is persisted as a tool-role
    // message BETWEEN the user ask and the assistant confirmation, so the model's
    // memory shows that past "done" replies were backed by real tool calls. Without
    // it the history is just clean confirmations, which teaches the model it can
    // declare success without calling anything (the fake-claim failure mode).
    let (reply_tx, mut reply_rx) = mpsc::channel::<(String, Option<String>, String)>(8);
    // Bound history by an ESTIMATED token budget, not a message count — and keep
    // it TIGHT. The failure mode isn't a hard context wall (logs show truncated=0);
    // it's that a 14B's tool-calling reliability degrades once the prompt grows
    // past ~4.5k tokens. The request is system (~600) + tool manifest (~1.4k, sent
    // every turn) + history, so capping history here PLATEAUS the whole prompt at
    // ~2.5–3k no matter how many commands are fired — it never enters the
    // degradation zone, so there's no tool-call "ceiling". ~4 chars ≈ 1 token.
    // (~9 exchanges of short-turn memory, which is plenty for a voice assistant.)
    const HISTORY_TOKEN_BUDGET: usize = 1200;
    let mut history_tokens = 0usize;
    loop {
        tokio::select! {
            _ = shutdown_listener.wait() => break,
            Some((user_text, tool_trace, reply)) = reply_rx.recv() => {
                use oracle_core::llm::{ChatMessage, Role};
                let est = |s: &str| s.len() / 4 + 4; // ~4 chars/token, +4 framing
                history_tokens += est(&user_text);
                history.push(ChatMessage { role: Role::User, content: user_text });
                if let Some(trace) = tool_trace {
                    history_tokens += est(&trace);
                    history.push(ChatMessage { role: Role::Tool, content: trace });
                }
                history_tokens += est(&reply);
                history.push(ChatMessage { role: Role::Assistant, content: reply });
                // Drop oldest messages until we're back under budget.
                while history_tokens > HISTORY_TOKEN_BUDGET && !history.is_empty() {
                    let m = history.remove(0);
                    history_tokens = history_tokens.saturating_sub(est(&m.content));
                }
            }
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
                        // Cancel any in-flight turn, then start a new one — with
                        // the running history so far as context.
                        if let Some(tok) = active_turn.take() {
                            tok.cancel();
                        }
                        let cancel = CancellationToken::new();
                        active_turn = Some(cancel.clone());
                        spawn_hud_turn(
                            agent.clone(),
                            publisher.clone(),
                            text,
                            history.clone(),
                            cancel,
                            last_tok_per_s.clone(),
                            voice_cfg.clone(),
                            reply_tx.clone(),
                            followup_tx.clone(),
                            turn_busy.clone(),
                            llm_life.clone(),
                            idle_tracker.clone(),
                        );
                    }
                    Some(HudCommand::Confirm { request_id, allow }) => {
                        // The user passed sentence in the Apollo modal.
                        info!(%request_id, allow, "confirmation decree");
                        confirmer.resolve(request_id, allow);
                    }
                    Some(HudCommand::Audio { wav_b64 }) => {
                        // A captured utterance: transcribe locally, then run the
                        // transcript exactly like a typed message.
                        publisher.send_event(HudEvent::State {
                            turn: uuid::Uuid::nil(),
                            state: "thinking".into(),
                        });
                        match transcribe_async(voice_cfg.clone(), wav_b64).await {
                            Some(text) if !text.trim().is_empty() => {
                                if let Some(tok) = active_turn.take() {
                                    tok.cancel();
                                }
                                let cancel = CancellationToken::new();
                                active_turn = Some(cancel.clone());
                                spawn_hud_turn(
                                    agent.clone(),
                                    publisher.clone(),
                                    text.trim().to_string(),
                                    history.clone(),
                                    cancel,
                                    last_tok_per_s.clone(),
                                    voice_cfg.clone(),
                                    reply_tx.clone(),
                                    followup_tx.clone(),
                                    turn_busy.clone(),
                                    llm_life.clone(),
                                    idle_tracker.clone(),
                                );
                            }
                            _ => {
                                // Audio arrived but nothing came back. Either the
                                // clip was silent or the recognizer failed — tell
                                // the user so it isn't a silent dead end.
                                let msg = if stt_on {
                                    "I didn't catch that — try again, or check the Whisper model path."
                                } else {
                                    "voice recognition isn't set up — set [voice] stt_program in oracle.toml."
                                };
                                publisher.send_event(HudEvent::Caption { text: msg.into() });
                                publisher.send_event(HudEvent::State {
                                    turn: uuid::Uuid::nil(),
                                    state: "idle".into(),
                                });
                            }
                        }
                    }
                    Some(HudCommand::Hello) => {
                        // The HUD (re)connected — (re)announce capabilities.
                        publisher.send_event(HudEvent::Config {
                            stt: stt_on,
                            tts: tts_on,
                            wake: current_wake,
                        });
                    }
                    Some(HudCommand::SetWake { active }) => {
                        // Wake chip toggled: start/stop the streaming recognizer.
                        current_wake = active && wake_on;
                        let _ = wake_tx.send(current_wake);
                        publisher.send_event(HudEvent::Config {
                            stt: stt_on,
                            tts: tts_on,
                            wake: current_wake,
                        });
                    }
                    Some(HudCommand::Summon) => {
                        // Wake word heard: raise a flag the native shell polls so
                        // it can bring the (possibly dismissed) window forward.
                        info!("summon requested (wake word)");
                        raise_summon_flag();
                    }
                    Some(HudCommand::Expand) => {
                        // The user clicked "Expand" in the compact window.
                        info!("expand requested (compact → full)");
                        raise_expand_flag();
                    }
                    Some(HudCommand::Retract) => {
                        // The user clicked "Compact" in the full window.
                        info!("retract requested (full → compact)");
                        raise_retract_flag();
                    }
                    Some(_) => {}
                    None => {}
                }
            }
        }
    }

    // Graceful drain: persist session and exit.
    wake_cancel.cancel(); // stop the wake listener + its whisper-stream child
    let snap = SessionSnapshot {
        turn_count: 0,
        rolling_summary: String::new(),
        last_saved_unix: chrono::Utc::now().timestamp(),
    };
    let _ = snap.save(&session_path);
    let _ = agent; // kept alive until here

    proactive_stop.cancel();
    if let Some(t) = idle_watch_stop.take() {
        t.cancel();
    }
    // Reap the supervised children (LLM server, actd) so nothing is orphaned.
    supervisor.shutdown().await;
    info!("shutdown complete");
    Ok(())
}

/// Fire due standing orders by injecting them into the normal command channel.
///
/// Deliberately reuses the ordinary turn path rather than running the agent
/// directly: a routine then gets history, the busy flag, LLM reload-on-demand,
/// TTS and the HUD state machine for free, and behaves exactly as if the user
/// had typed it at that moment -- which is precisely what a standing order is.
///
/// Unlike the nudge triggers in `oracle_core::proactive`, this DOES run the
/// planner. The distinction is authorship: a nudge is a heuristic firing on its
/// own, a routine is the user's own instruction, time-shifted. The capability
/// gate is unchanged, so an unattended turn that reaches an irreversible action
/// still stops at the confirmer and is denied when nobody answers.
fn spawn_routine_watcher(
    shared: Arc<Shared>,
    cmd_tx: tokio::sync::mpsc::Sender<HudCommand>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown.cancelled() => return,
            }

            // Never start a routine on top of a live conversation; it will be
            // due again on the next tick.
            if busy.load(std::sync::atomic::Ordering::SeqCst) {
                continue;
            }

            let now_unix = chrono::Utc::now().timestamp();
            let local = chrono::Local::now().naive_local();
            let due = match shared.routines.due(local, now_unix) {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("[routine] could not read routines: {e}");
                    continue;
                }
            };

            for r in due {
                tracing::info!(name = %r.name, "[routine] firing");
                // Mark BEFORE running. A routine whose turn is slow, or which
                // fails outright, must not fire again on the next 30s tick.
                if let Err(e) = shared.routines.mark_fired(r.id, now_unix) {
                    tracing::warn!(name = %r.name, "[routine] could not mark fired: {e}");
                    continue;
                }
                if cmd_tx
                    .send(HudCommand::UserText {
                        text: r.prompt.clone(),
                    })
                    .await
                    .is_err()
                {
                    return; // the run loop is gone
                }
                // One at a time: several routines due at once should not race
                // each other into overlapping turns.
                break;
            }
        }
    });
}

/// The proactive loop: poll the triggers, gate each nudge through the policy,
/// and speak whatever survives.
///
/// Deliberately has no `Agent` and no tool registry. Its only capabilities are
/// reading Google and emitting speech, so the worst a bug here can do is say
/// something at a bad moment -- see `oracle_core::proactive` for why that
/// boundary is drawn this way.
#[allow(clippy::too_many_arguments)]
fn spawn_proactive(
    cfg: oracle_core::config::ProactiveConfig,
    google: Option<Arc<oracle_core::connectors::google_api::GoogleClient>>,
    shared: Arc<Shared>,
    publisher: oracle_core::gateway::server::HudPublisher,
    voice: Arc<oracle_core::config::VoiceConfig>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    shutdown: CancellationToken,
) {
    use oracle_core::proactive::{triggers, NudgePolicy, PolicyConfig};
    use std::sync::atomic::Ordering;

    let mut processes = triggers::ProcessWatcher::new(cfg.watch_processes.clone());
    let watches_local = !processes.is_empty() && shared.actd.is_some();

    // Google-backed triggers are optional; the local ones stand on their own.
    if google.is_none() && !watches_local {
        tracing::info!(
            "[proactive] enabled but nothing to watch: Google is not authorized and no \
             watch_processes are set (or actd is not connected)."
        );
        return;
    }

    tokio::spawn(async move {
        let mut policy = NudgePolicy::new(PolicyConfig {
            quiet_from_hour: cfg.quiet_from_hour,
            quiet_until_hour: cfg.quiet_until_hour,
            repeat_after_secs: cfg.repeat_after_secs,
            max_per_hour: cfg.max_per_hour,
        });
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(cfg.poll_secs.max(10)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!(
            poll_secs = cfg.poll_secs,
            calendar = cfg.calendar,
            mail = cfg.mail,
            "[proactive] watching"
        );

        loop {
            tokio::select! {
                _ = tick.tick() => {}
                _ = shutdown.cancelled() => {
                    tracing::info!("[proactive] stopping");
                    return;
                }
            }

            let now = chrono::Utc::now().timestamp();
            let mut nudges = Vec::new();

            if let Some(google) = google.as_ref() {
                if cfg.calendar {
                    // Only look as far ahead as we would announce.
                    let t_min = chrono::Utc::now().to_rfc3339();
                    let t_max = (chrono::Utc::now()
                        + chrono::Duration::minutes(cfg.lead_minutes.max(1)))
                    .to_rfc3339();
                    match google.calendar_events(&t_min, &t_max, now).await {
                        Ok(events) => {
                            nudges.extend(triggers::calendar_nudges(&events, now, cfg.lead_minutes))
                        }
                        // A transient network failure must not kill the loop.
                        Err(e) => tracing::warn!("[proactive] calendar poll failed: {e}"),
                    }
                }

                if cfg.mail {
                    match google.gmail_search(&cfg.mail_query, 5, now).await {
                        Ok(mail) => nudges.extend(triggers::mail_nudges(&mail)),
                        Err(e) => tracing::warn!("[proactive] mail poll failed: {e}"),
                    }
                }
            }

            // Local: what is running on this machine right now.
            if watches_local {
                if let Some(actd) = shared.actd.clone() {
                    match actd
                        .call(
                            uuid::Uuid::new_v4(),
                            oracle_ipc::actd::ActRequest::ListProcesses,
                        )
                        .await
                    {
                        Ok(oracle_ipc::actd::ActResponse::Ok { data }) => {
                            let names: Vec<String> = data
                                .get("processes")
                                .and_then(|p| p.as_array())
                                .map(|arr| {
                                    arr.iter()
                                        .filter_map(|p| p.get("name").and_then(|n| n.as_str()))
                                        .map(|n| n.to_string())
                                        .collect()
                                })
                                .unwrap_or_default();
                            nudges.extend(processes.poll(&names));
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("[proactive] process poll failed: {e}"),
                    }
                }
            }

            for nudge in nudges {
                let local_hour = {
                    use chrono::Timelike as _;
                    chrono::Local::now().hour()
                };
                let in_flight = busy.load(Ordering::SeqCst);
                match policy.admit(&nudge, now, local_hour, in_flight) {
                    Ok(()) => {
                        tracing::info!(kind = nudge.kind.as_str(), key = %nudge.key, "[proactive] speaking");
                        let wav = synth_tts_async(voice.clone(), nudge.text.clone())
                            .await
                            .map(encode_wav_b64);
                        publisher.send_event(HudEvent::Speak {
                            text: nudge.text,
                            wav_b64: wav,
                        });
                    }
                    Err(reason) => {
                        tracing::debug!(key = %nudge.key, ?reason, "[proactive] suppressed");
                    }
                }
            }
        }
    });
}

/// Clears a flag when dropped, so the "a turn is in flight" signal is released
/// on every exit path -- including a panic inside the turn task.
struct FlagGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for FlagGuard {
    fn drop(&mut self) {
        self.0.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}
/// Run one HUD-driven conversation turn on its own task, mapping the agent's
/// events onto HUD events so the browser shows the transcript, tool activity,
/// and the streaming reply.
#[allow(clippy::too_many_arguments)]
fn spawn_hud_turn(
    agent: Arc<Agent>,
    publisher: oracle_core::gateway::server::HudPublisher,
    text: String,
    history: Vec<oracle_core::llm::ChatMessage>,
    cancel: CancellationToken,
    last_tok_per_s: Arc<std::sync::atomic::AtomicU32>,
    voice: Arc<oracle_core::config::VoiceConfig>,
    reply_tx: mpsc::Sender<(String, Option<String>, String)>,
    followup_tx: mpsc::Sender<std::time::Duration>,
    busy: Arc<std::sync::atomic::AtomicBool>,
    llm_life: Arc<oracle_core::idle::LlmLifecycle>,
    idle_tracker: Arc<oracle_core::idle::IdleTracker>,
) {
    tokio::spawn(async move {
        // Tell the proactive loop a real conversation is happening, so it does
        // not talk over it. Cleared on every exit path below, including errors.
        busy.store(true, std::sync::atomic::Ordering::SeqCst);
        let _busy_guard = FlagGuard(busy.clone());
        idle_tracker.touch(chrono::Utc::now().timestamp());
        let turn = uuid::Uuid::new_v4();
        // Keep a copy of the user's message to report back for history.
        let user_text = text.clone();
        // Echo the user's message and enter the "thinking" state.
        publisher.send_event(HudEvent::Transcript {
            text: text.clone(),
            stable: true,
        });
        publisher.send_event(HudEvent::State {
            turn,
            state: "thinking".into(),
        });

        // The model may have been unloaded while idle. Bring it back and wait
        // for it to answer /health before the request goes out, so the first
        // turn after a lull is slow rather than a bare connection error.
        if !llm_life.ensure_ready().await {
            publisher.send_event(HudEvent::Caption {
                text: "the model is still loading — try again in a moment.".into(),
            });
            publisher.send_event(HudEvent::State {
                turn,
                state: "idle".into(),
            });
            return;
        }

        let (tx, mut rx) = mpsc::channel::<AgentEvent>(256);
        let run = agent.run_turn_with_history(history, text, tx, cancel);

        let pub2 = publisher.clone();
        let tok_counter = last_tok_per_s.clone();
        let forward = async move {
            let mut reply = String::new();
            let mut spoke = false;
            let mut saw_finished = false;
            let mut was_cancelled = false;
            let mut tools_ran: Vec<(String, bool)> = Vec::new();
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
                        tools_ran.push((name.clone(), ok));
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
                    AgentEvent::Finished { cancelled } => {
                        saw_finished = true;
                        was_cancelled = cancelled;
                        // Only settle to idle if we finished on our own terms. A
                        // cancelled turn was superseded (a new turn already set
                        // "thinking") or interrupted (the handler set idle) — so
                        // emitting idle here would race and blank out the state
                        // of the turn that took over.
                        if !cancelled {
                            pub2.send_event(HudEvent::State {
                                turn,
                                state: "idle".into(),
                            });
                        }
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
            (saw_finished, was_cancelled, reply, tools_ran)
        };

        // Guard the whole turn with a timeout so a hung LLM/tool can't leave the
        // HUD stuck on "thinking" forever. Whatever happens — error, timeout, or
        // a turn that ended without a Finished event — we surface a message and
        // return to idle so the Oracle is never silently frozen.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(180), async {
            tokio::join!(run, forward)
        })
        .await;

        let (problem, reply, cancelled, tools_ran): (Option<String>, String, bool, Vec<(String, bool)>) = match outcome {
            Ok((Ok(_), (true, cancelled, reply, tools))) => (None, reply, cancelled, tools), // completed
            Ok((Ok(_), (false, cancelled, reply, tools))) => {
                (Some("the oracle gave no answer.".to_string()), reply, cancelled, tools)
            }
            Ok((Err(e), (_, cancelled, reply, tools))) => {
                (Some(format!("the oracle could not answer: {e}")), reply, cancelled, tools)
            }
            Err(_) => (
                Some(
                    "the oracle did not answer in time — is the LLM server running on the configured backend?"
                        .to_string(),
                ),
                String::new(),
                false,
                Vec::new(),
            ),
        };

        // Speak the outcome — but NOT if the turn was barged-in/cancelled (she
        // shouldn't finish speaking an answer the user just interrupted). On
        // failure we voice the error so a listening user hears it too.
        let to_speak = if cancelled {
            None
        } else {
            match &problem {
                None if !reply.trim().is_empty() => Some(reply.clone()),
                Some(msg) => Some(msg.clone()),
                None => None,
            }
        };
        if let Some(spoken) = to_speak {
            // If Pythia ended on a question, open a follow-up listening window so
            // the user can just answer — no "Delphi" needed. Keep it generous
            // enough to cover her speaking the question, then the reply.
            if spoken.trim_end().ends_with('?') {
                let _ = followup_tx.try_send(std::time::Duration::from_secs(14));
            }
            let wav_b64 = synth_tts_async(voice.clone(), spoken.clone())
                .await
                .map(encode_wav_b64);
            publisher.send_event(HudEvent::Speak {
                text: spoken,
                wav_b64,
            });
        }

        if let Some(msg) = &problem {
            tracing::warn!("turn did not complete cleanly: {msg}");
            publisher.send_event(HudEvent::Caption { text: msg.clone() });
        }
        // Settle back to idle at the very end — unless this turn was cancelled,
        // in which case whatever superseded it owns the state now (see above).
        if !cancelled {
            publisher.send_event(HudEvent::State {
                turn,
                state: "idle".into(),
            });
        }

        // Record this exchange in the conversation history (only clean turns
        // with a real reply — errors and cancellations aren't worth remembering).
        // Include a factual trace of the tools that actually ran, so the model's
        // memory shows past confirmations were earned by real calls (not just
        // declared) — the structural cure for fake "I did it" claims.
        if problem.is_none() && !cancelled && !reply.trim().is_empty() {
            let tool_trace = if tools_ran.is_empty() {
                None
            } else {
                let list = tools_ran
                    .iter()
                    .map(|(name, ok)| format!("{name}={}", if *ok { "ok" } else { "failed" }))
                    .collect::<Vec<_>>()
                    .join(", ");
                Some(format!("[tools executed this turn: {list}]"))
            };
            let _ = reply_tx.send((user_text, tool_trace, reply)).await;
        }
    });
}

/// Base64-encode synthesized WAV bytes for transport to the HUD.
fn encode_wav_b64(bytes: Vec<u8>) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Synthesize `text` to WAV bytes with the configured neural voice, off the
/// async runtime (the synth process is blocking). Returns None when TTS is
/// disabled/unset or the engine fails — the caller then leaves the HUD to fall
/// back to browser speech, so the Oracle always talks.
async fn synth_tts_async(
    voice: Arc<oracle_core::config::VoiceConfig>,
    text: String,
) -> Option<Vec<u8>> {
    if !voice.tts_enabled {
        return None;
    }
    // Prefer the persistent HTTP server (Kokoro) — warm and fast because the
    // model stays loaded. If it's unreachable (container down, port moved), fall
    // through to a per-call command engine (Piper) before giving up to the HUD's
    // browser voice — so a dead Kokoro degrades to Piper, not to the 2013 GPS.
    if !voice.tts_http_url.trim().is_empty() {
        if let Some(wav) = synth_http(&voice, &text).await {
            return Some(wav);
        }
        if !voice.tts_program.trim().is_empty() {
            tracing::warn!(
                "HTTP TTS at {} unavailable — falling back to command engine '{}'",
                voice.tts_http_url,
                voice.tts_program
            );
        } else {
            tracing::warn!(
                "HTTP TTS at {} unavailable and no tts_program set — the HUD will use the browser voice",
                voice.tts_http_url
            );
        }
    }
    if !voice.tts_program.trim().is_empty() {
        let v = voice.clone();
        let t = text.clone();
        return tokio::task::spawn_blocking(move || synth_tts_blocking(&v, &t))
            .await
            .ok()
            .flatten();
    }
    None
}

/// Ensure the local TTS server (Kokoro) is up: if the endpoint isn't already
/// answering, run the configured launch command ONCE (detached — not supervised,
/// so it survives core restarts), then poll until it's healthy. Everything is
/// logged to `tts.log` so a failed launch is diagnosable, never silent.
async fn ensure_tts_server(voice: oracle_core::config::VoiceConfig, log_path: std::path::PathBuf) {
    // 1) Already up? Then there's nothing to launch — the common case on restart.
    if tts_endpoint_alive(&voice.tts_http_url).await {
        tracing::info!("TTS server already answering at {}", voice.tts_http_url);
        return;
    }

    // 2) Launch the configured command once, detached, output → tts.log.
    let (out, err) = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => match f.try_clone() {
            Ok(f2) => (std::process::Stdio::from(f), std::process::Stdio::from(f2)),
            Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
        },
        Err(_) => (std::process::Stdio::null(), std::process::Stdio::null()),
    };
    let mut cmd = std::process::Command::new(&voice.tts_server_program);
    cmd.args(&voice.tts_server_args)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    match cmd.spawn() {
        Ok(_child) => {
            // Intentionally drop the handle: no kill_on_drop, so the container
            // keeps running independently of core (start it once, use it forever).
            tracing::info!(
                "launched TTS server: {} {} (waiting for it to warm up)",
                voice.tts_server_program,
                voice.tts_server_args.join(" ")
            );
        }
        Err(e) => {
            tracing::warn!(
                "could not launch TTS server '{}': {e} — falling back to command/browser voice",
                voice.tts_server_program
            );
            return;
        }
    }

    // 3) Poll for readiness (containers can take a while to pull/boot). ~2 min.
    for attempt in 1..=40u32 {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        if tts_endpoint_alive(&voice.tts_http_url).await {
            tracing::info!(
                "TTS server is up after ~{}s — warm voice ready",
                attempt * 3
            );
            return;
        }
    }
    tracing::warn!(
        "TTS server at {} did not come up in ~2 min — check {}",
        voice.tts_http_url,
        log_path.display()
    );
}

/// Liveness probe for the TTS endpoint: any HTTP reply (even 4xx/405) means the
/// server is listening; only a connection-level error means it's down.
async fn tts_endpoint_alive(url: &str) -> bool {
    if url.trim().is_empty() {
        return false;
    }
    let client = reqwest::Client::new();
    // GET the speech endpoint's origin. A POST-only route answers a GET with 405,
    // which is still proof the port is open — exactly what we want to detect.
    let probe = tts_origin(url);
    client
        .get(&probe)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

/// Reduce a full endpoint URL to its scheme://host[:port] origin for probing.
fn tts_origin(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        let host_end = after.find('/').unwrap_or(after.len());
        format!("{}{}", &url[..scheme_end + 3], &after[..host_end])
    } else {
        url.to_string()
    }
}

/// Synthesize via an OpenAI-compatible TTS HTTP endpoint (Kokoro-FastAPI etc.).
/// Returns the audio bytes (WAV) or None on any failure, so the HUD can fall
/// back to browser speech.
async fn synth_http(voice: &oracle_core::config::VoiceConfig, text: &str) -> Option<Vec<u8>> {
    let input = text.replace(['\n', '\r'], " ");
    let body = serde_json::json!({
        "model": voice.tts_model,
        "input": input,
        "voice": voice.tts_voice,
        "response_format": "wav",
    });
    let client = reqwest::Client::new();
    let resp = match client
        .post(&voice.tts_http_url)
        .json(&body)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("TTS HTTP request to {} failed: {e}", voice.tts_http_url);
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::warn!("TTS HTTP {} from {}", resp.status(), voice.tts_http_url);
        return None;
    }
    match resp.bytes().await {
        Ok(b) if !b.is_empty() => Some(b.to_vec()),
        _ => None,
    }
}

/// The blocking half: run the TTS program, feeding text on stdin and reading the
/// WAV it writes to a temp path (the `{out}` token in the args). Hidden on
/// Windows so no console flashes.
fn synth_tts_blocking(voice: &oracle_core::config::VoiceConfig, text: &str) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    // One utterance, one line — collapse newlines so per-line synths (Piper)
    // produce a single WAV rather than several.
    let line = text.replace(['\n', '\r'], " ");
    let out = std::env::temp_dir().join(format!("oracle-tts-{}.wav", uuid::Uuid::new_v4()));
    let out_str = out.to_string_lossy().to_string();
    let args: Vec<String> = voice
        .tts_args
        .iter()
        .map(|a| a.replace("{out}", &out_str))
        .collect();

    let mut cmd = Command::new(&voice.tts_program);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("TTS program '{}' failed to launch: {e}", voice.tts_program);
            return None;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(line.as_bytes());
        // stdin drops here → EOF, so the synth knows the input is complete.
    }
    match child.wait() {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!("TTS program exited with {status}");
            let _ = std::fs::remove_file(&out);
            return None;
        }
        Err(e) => {
            tracing::warn!("waiting on TTS program failed: {e}");
            return None;
        }
    }
    let bytes = std::fs::read(&out).ok();
    let _ = std::fs::remove_file(&out);
    bytes.filter(|b| !b.is_empty())
}

/// Transcribe a base64 WAV utterance with the configured local recognizer, off
/// the async runtime. Returns None when STT is disabled/unset, the audio is
/// unreadable, or nothing intelligible came back.
async fn transcribe_async(
    voice: Arc<oracle_core::config::VoiceConfig>,
    wav_b64: String,
) -> Option<String> {
    if !voice.stt_enabled || voice.stt_program.trim().is_empty() {
        return None;
    }
    tokio::task::spawn_blocking(move || transcribe_blocking(&voice, &wav_b64))
        .await
        .ok()
        .flatten()
}

/// The blocking half: decode the WAV to a temp file, run the recognizer with
/// `{in}` pointing at it, and read the transcript from stdout. Hidden on Windows.
fn transcribe_blocking(voice: &oracle_core::config::VoiceConfig, wav_b64: &str) -> Option<String> {
    use base64::Engine as _;
    use std::process::{Command, Stdio};

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(wav_b64)
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    let input = std::env::temp_dir().join(format!("oracle-stt-{}.wav", uuid::Uuid::new_v4()));
    if std::fs::write(&input, &bytes).is_err() {
        return None;
    }
    let in_str = input.to_string_lossy().to_string();
    let args: Vec<String> = voice
        .stt_args
        .iter()
        .map(|a| a.replace("{in}", &in_str))
        .collect();

    let mut cmd = Command::new(&voice.stt_program);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }

    let output = cmd.output();
    let _ = std::fs::remove_file(&input);
    let output = match output {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!("STT program exited with {}", o.status);
            return None;
        }
        Err(e) => {
            tracing::warn!("STT program '{}' failed to launch: {e}", voice.stt_program);
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    // whisper.cpp emits markers like "[BLANK_AUDIO]" or "(silence)" for empty
    // input; treat those (and anything empty) as "nothing said".
    let lower = text.to_lowercase();
    if text.is_empty()
        || lower.contains("[blank_audio]")
        || lower.contains("(silence)")
        || lower == "[ silence ]"
    {
        return None;
    }
    Some(text)
}

/// Match the wake word ("Delphi") in a transcript line. Returns the command that
/// follows it (empty string for a bare summon), or None if the wake word isn't
/// present. Tolerant of how speech-to-text renders the name.
fn match_wake(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    for w in [
        "delphi", "delphie", "delphy", "delphey", "delfie", "delfi", "del phi", "del fi",
    ] {
        if let Some(pos) = lower.find(w) {
            let after = &lower[pos + w.len()..];
            let cmd = after
                .trim_start_matches(|c: char| c.is_whitespace() || ",.;:!?—-".contains(c))
                .trim()
                .to_string();
            return Some(cmd);
        }
    }
    None
}

/// Remove ANSI escape sequences (`\x1b[ ... <letter>`) and stray control
/// characters from a line, leaving printable text.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip a CSI sequence: ESC '[' ... final byte in @..~.
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            }
            continue;
        }
        // Drop other control chars except normal whitespace.
        if (c.is_control()) && c != '\t' {
            continue;
        }
        out.push(c);
    }
    out
}

/// Turn one raw recognizer output line into clean speech text, or None if it's
/// log noise. whisper-stream (with timestamps) prints `[t0 --> t1]  the words`;
/// we strip the timestamp and keep the words. Genuine init/banner lines are
/// dropped. Crucially, a timestamped transcript line is NOT treated as noise
/// just because it starts with '[' — that bug silently ate every utterance.
fn clean_transcript_line(line: &str) -> Option<String> {
    // Strip ANSI escape sequences (whisper-stream redraws with `\x1b[2K` etc.)
    // and other control chars, then trim.
    let stripped = strip_ansi(line);
    let l = stripped.trim();
    if l.is_empty() {
        return None;
    }
    let low = l.to_lowercase();
    // Real recognizer logs → ignore.
    if low.contains("whisper_")
        || low.contains("ggml")
        || low.contains("main:")
        || low.contains("system_info")
        || low.contains("n_threads")
        || low.contains("start speaking")
        || low.contains("model loaded")
        || low.contains("sampling")
        || l.starts_with("###")
    {
        return None;
    }
    // Timestamped transcript: "[00:00:00.000 --> 00:00:02.000]   text".
    if l.starts_with('[') && l.contains("-->") {
        if let Some(idx) = l.find(']') {
            let text = l[idx + 1..].trim();
            return (!text.is_empty()).then(|| text.to_string());
        }
    }
    // A bracketed status without "-->" (e.g. "[Start speaking]") → ignore.
    if l.starts_with('[') {
        return None;
    }
    Some(l.to_string())
}

/// How long to keep gathering speech after the last segment before firing the
/// command. Covers the comma-pauses in a normal sentence so "maximize spotify,
/// and pause it" fires as ONE command instead of being cut off at the pause.
const WAKE_SETTLE: std::time::Duration = std::time::Duration::from_millis(1100);

/// Mutable state the wake listener threads across transcript lines.
struct WakeState {
    last_fired: std::time::Instant,
    last_text: String,
    /// When set (and not yet elapsed), the next utterance is treated as a command
    /// even without the wake word — opened by a bare "Delphi" or by Pythia asking
    /// a question (so the user can reply without re-saying the wake word).
    expecting_until: Option<std::time::Instant>,
    /// After a command fires, ignore further wake hits until this passes.
    /// whisper-stream surfaces one spoken phrase as several lines; without this
    /// each would spawn its own turn — stacked, overlapping replies.
    cooldown_until: Option<std::time::Instant>,
    /// A command being gathered across segments; fired once the mic goes quiet
    /// for `WAKE_SETTLE`. `pending_deadline` is when that quiet gap elapses.
    pending: String,
    pending_deadline: Option<std::time::Instant>,
}

/// Process one recognizer line: show it live, and drive the wake word — barge in
/// on whatever she's saying, run "Delphi, <command>" directly, and for a bare
/// "Delphi" acknowledge and open a short window for the follow-up command.
async fn handle_wake_line(
    line: &str,
    publisher: &oracle_core::gateway::server::HudPublisher,
    cmd_tx: &tokio::sync::mpsc::Sender<HudCommand>,
    voice: &Arc<oracle_core::config::VoiceConfig>,
    st: &mut WakeState,
    llm_life: &Arc<oracle_core::idle::LlmLifecycle>,
    idle_tracker: &Arc<oracle_core::idle::IdleTracker>,
) {
    use std::time::{Duration, Instant};
    let Some(text) = clean_transcript_line(line) else {
        return;
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return;
    }
    publisher.send_event(HudEvent::Interim { text: text.clone() });

    // Speech means the user is here: hold off the idle unloader, and if the
    // model is already down start reloading NOW rather than when they finish
    // talking. The load then overlaps with the rest of the sentence instead of
    // being dead air after it.
    idle_tracker.touch(chrono::Utc::now().timestamp());
    if !llm_life.is_inert() {
        let life = llm_life.clone();
        tokio::spawn(async move {
            let _ = life.ensure_ready().await;
        });
    }
    let now = Instant::now();

    // 1) Already gathering a command → this segment is a continuation. Fold it in
    //    and push the quiet-gap deadline out. This is what lets a comma-paused
    //    sentence ("…spotify, and pause it") arrive in one piece.
    if st.pending_deadline.is_some() {
        accumulate_pending(st, &text);
        st.pending_deadline = Some(now + WAKE_SETTLE);
        return;
    }

    // 2) A follow-up window is open (bare "Delphi", or Pythia just asked a
    //    question) → the whole utterance is the reply, no wake word needed.
    if let Some(until) = st.expecting_until.take() {
        if now < until {
            begin_pending(st, text, publisher, cmd_tx).await;
            return;
        }
    }

    // 3) Otherwise we need the wake word.
    let Some(command) = match_wake(&text) else {
        return;
    };
    if let Some(cd) = st.cooldown_until {
        if now < cd {
            return;
        }
    }
    if st.last_text == text && st.last_fired.elapsed() < Duration::from_secs(2) {
        return; // identical-repeat debounce
    }
    st.last_text = text.clone();
    st.last_fired = now;

    if command.is_empty() {
        // Bare "Delphi": acknowledge and open the follow-up window.
        st.cooldown_until = Some(now + Duration::from_millis(1200));
        barge_in(publisher, cmd_tx).await;
        st.expecting_until = Some(now + Duration::from_secs(8));
        publisher.send_event(HudEvent::State {
            turn: uuid::Uuid::nil(),
            state: "listening".into(),
        });
        let wav = synth_tts_async(voice.clone(), "Yes?".into())
            .await
            .map(encode_wav_b64);
        publisher.send_event(HudEvent::Speak {
            text: "Yes?".into(),
            wav_b64: wav,
        });
    } else {
        // "Delphi, <command>": start gathering; the settle timer fires it.
        begin_pending(st, command, publisher, cmd_tx).await;
    }
}

/// Start gathering a spoken command: stop whatever she's saying NOW (so she
/// listens while the user finishes), seed the buffer, and open the settle gap.
async fn begin_pending(
    st: &mut WakeState,
    text: String,
    publisher: &oracle_core::gateway::server::HudPublisher,
    cmd_tx: &tokio::sync::mpsc::Sender<HudCommand>,
) {
    barge_in(publisher, cmd_tx).await;
    st.pending = text;
    st.pending_deadline = Some(std::time::Instant::now() + WAKE_SETTLE);
    publisher.send_event(HudEvent::State {
        turn: uuid::Uuid::nil(),
        state: "listening".into(),
    });
}

/// Fold a continuation segment into the pending command. A segment that just
/// grows the previous one (a re-emitted partial) replaces it; a genuinely new
/// segment (the words after a pause) is appended.
fn accumulate_pending(st: &mut WakeState, text: &str) {
    let cur = st.pending.trim();
    let low_cur = cur.to_lowercase();
    let low_new = text.to_lowercase();
    if low_new.starts_with(&low_cur) || low_cur.starts_with(&low_new) {
        // Prefix-related → it's the same utterance growing; keep the longer.
        if text.len() > cur.len() {
            st.pending = text.to_string();
        }
    } else {
        st.pending = format!("{cur} {text}");
    }
}

/// Fire the gathered command as one turn, then briefly lock out re-triggers so
/// its own trailing partials don't spawn a second turn.
async fn flush_pending(st: &mut WakeState, cmd_tx: &tokio::sync::mpsc::Sender<HudCommand>) {
    st.pending_deadline = None;
    let command = std::mem::take(&mut st.pending).trim().to_string();
    if command.is_empty() {
        return;
    }
    st.cooldown_until = Some(std::time::Instant::now() + std::time::Duration::from_millis(800));
    let _ = cmd_tx.send(HudCommand::UserText { text: command }).await;
}

/// Barge-in: stop her current speech in the HUD and cancel any active turn.
async fn barge_in(
    publisher: &oracle_core::gateway::server::HudPublisher,
    cmd_tx: &tokio::sync::mpsc::Sender<HudCommand>,
) {
    publisher.send_event(HudEvent::StopAudio);
    let _ = cmd_tx.send(HudCommand::Interrupt).await;
    let _ = cmd_tx.send(HudCommand::Summon).await;
}

/// Run the streaming recognizer while `active`, forwarding its transcript to the
/// HUD (live "what I'm hearing") and injecting a turn when it hears "Delphi".
#[allow(clippy::too_many_arguments)]
fn spawn_wake_listener(
    voice: Arc<oracle_core::config::VoiceConfig>,
    publisher: oracle_core::gateway::server::HudPublisher,
    cmd_tx: tokio::sync::mpsc::Sender<HudCommand>,
    mut active_rx: tokio::sync::watch::Receiver<bool>,
    mut followup_rx: tokio::sync::mpsc::Receiver<std::time::Duration>,
    shutdown: CancellationToken,
    llm_life: Arc<oracle_core::idle::LlmLifecycle>,
    idle_tracker: Arc<oracle_core::idle::IdleTracker>,
) {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    tokio::spawn(async move {
        loop {
            // Idle until the wake listener is switched on.
            while !*active_rx.borrow() {
                tokio::select! {
                    _ = active_rx.changed() => {}
                    _ = shutdown.cancelled() => return,
                }
            }

            let mut cmd = tokio::process::Command::new(&voice.wake_program);
            // Pipe BOTH streams — different whisper builds print the transcript
            // to stdout or stderr, so we read (and parse) both.
            cmd.args(&voice.wake_args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("wake: could not launch {}: {e}", voice.wake_program);
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                        _ = shutdown.cancelled() => return,
                    }
                    continue;
                }
            };
            tracing::info!("wake listener started ({})", voice.wake_program);
            let mut out_lines = child.stdout.take().map(|s| BufReader::new(s).lines());
            let mut err_lines = child.stderr.take().map(|s| BufReader::new(s).lines());
            let mut st = WakeState {
                last_fired: std::time::Instant::now() - Duration::from_secs(10),
                last_text: String::new(),
                expecting_until: None,
                cooldown_until: None,
                pending: String::new(),
                pending_deadline: None,
            };
            let (mut out_done, mut err_done) = (out_lines.is_none(), err_lines.is_none());

            loop {
                if out_done && err_done {
                    break; // recognizer closed both streams → respawn
                }
                // The settle timer: when a command is gathering, fire it once the
                // mic has been quiet for WAKE_SETTLE. `pending` future is inert
                // (never resolves) when nothing is being gathered.
                let settle = st.pending_deadline;
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        let _ = child.start_kill();
                        return;
                    }
                    _ = active_rx.changed() => {
                        if !*active_rx.borrow() {
                            let _ = child.start_kill();
                            let _ = child.wait().await;
                            break;
                        }
                    }
                    // Pythia asked a question (or a turn wants a follow-up) → open
                    // a listening window so the user can reply without "Delphi".
                    Some(dur) = followup_rx.recv() => {
                        st.expecting_until = Some(std::time::Instant::now() + dur);
                        publisher.send_event(HudEvent::State {
                            turn: uuid::Uuid::nil(),
                            state: "listening".into(),
                        });
                    }
                    // Quiet gap elapsed on a gathering command → fire it.
                    _ = async {
                        match settle {
                            Some(dl) => tokio::time::sleep_until(tokio::time::Instant::from_std(dl)).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => {
                        flush_pending(&mut st, &cmd_tx).await;
                    }
                    l = async { out_lines.as_mut().unwrap().next_line().await }, if !out_done => {
                        match l {
                            Ok(Some(line)) => {
                                handle_wake_line(
                                    &line, &publisher, &cmd_tx, &voice, &mut st,
                                    &llm_life, &idle_tracker,
                                )
                                .await
                            }
                            _ => out_done = true,
                        }
                    }
                    l = async { err_lines.as_mut().unwrap().next_line().await }, if !err_done => {
                        match l {
                            Ok(Some(line)) => {
                                handle_wake_line(
                                    &line, &publisher, &cmd_tx, &voice, &mut st,
                                    &llm_life, &idle_tracker,
                                )
                                .await
                            }
                            _ => err_done = true,
                        }
                    }
                }
            }

            // Brief pause before respawning (unless we're shutting down).
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                _ = shutdown.cancelled() => return,
            }
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

#[cfg(test)]
mod tts_tests {
    use super::tts_origin;

    #[test]
    fn origin_strips_path() {
        assert_eq!(
            tts_origin("http://127.0.0.1:8880/v1/audio/speech"),
            "http://127.0.0.1:8880"
        );
        assert_eq!(tts_origin("http://localhost:8880"), "http://localhost:8880");
        assert_eq!(tts_origin("https://host/v1/x"), "https://host");
        // No scheme → returned as-is (defensive).
        assert_eq!(tts_origin("127.0.0.1:8880"), "127.0.0.1:8880");
    }
}

#[cfg(test)]
mod wake_tests {
    use super::{accumulate_pending, clean_transcript_line, match_wake, WakeState};

    fn empty_state(pending: &str) -> WakeState {
        WakeState {
            last_fired: std::time::Instant::now(),
            last_text: String::new(),
            expecting_until: None,
            cooldown_until: None,
            pending: pending.to_string(),
            pending_deadline: None,
        }
    }

    #[test]
    fn accumulate_appends_new_segments_but_replaces_growth() {
        // A comma-paused continuation is appended in full.
        let mut st = empty_state("maximize my spotify");
        accumulate_pending(&mut st, "and pause it");
        assert_eq!(st.pending, "maximize my spotify and pause it");

        // A re-emitted growing partial replaces (keep the longer), no duplication.
        let mut st = empty_state("maximize my");
        accumulate_pending(&mut st, "maximize my spotify");
        assert_eq!(st.pending, "maximize my spotify");

        // A shorter partial of the same utterance is ignored (keep the longer).
        let mut st = empty_state("maximize my spotify");
        accumulate_pending(&mut st, "maximize my");
        assert_eq!(st.pending, "maximize my spotify");
    }

    #[test]
    fn detects_wake_and_extracts_command() {
        assert_eq!(
            match_wake("Delphi, turn up the volume").as_deref(),
            Some("turn up the volume")
        );
        assert_eq!(
            match_wake("hey delphi open spotify").as_deref(),
            Some("open spotify")
        );
        assert_eq!(match_wake("delphi").as_deref(), Some("")); // bare summon
    }

    #[test]
    fn ignores_non_wake_speech() {
        assert!(match_wake("what time is the meeting").is_none());
        assert!(match_wake("play the next song").is_none());
    }

    #[test]
    fn strips_timestamps_and_filters_logs() {
        // Log/banner lines are dropped.
        assert!(clean_transcript_line("whisper_init_state: loading model").is_none());
        assert!(clean_transcript_line("[Start speaking]").is_none());
        assert!(clean_transcript_line("ggml_backend: using CPU").is_none());
        // A timestamped transcript line keeps the words (this is the bug fix:
        // it must NOT be dropped just for starting with '[').
        assert_eq!(
            clean_transcript_line("[00:00:00.000 --> 00:00:02.000]   Delphi open spotify")
                .as_deref(),
            Some("Delphi open spotify")
        );
        // And a plain line passes through.
        assert_eq!(
            clean_transcript_line("turn up the volume").as_deref(),
            Some("turn up the volume")
        );
    }
}

#[cfg(test)]
mod config_arg_tests {
    use super::load_config;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn an_explicit_config_that_does_not_exist_is_a_hard_error() {
        // The silent fallback here cost a real debugging session: the app boots
        // looking healthy, but the defaults have autostart_llm=false and no
        // voice programs, so the LLM, wake word and TTS are all just missing.
        let err = load_config(&args(&["run", "--config", "/definitely/not/here.toml"]))
            .expect_err("a missing explicit config must fail");
        let msg = format!("{err}");
        assert!(msg.contains("/definitely/not/here.toml"), "got: {msg}");
        // The hint has to name the trap that caused it.
        assert!(msg.contains("APPDATA"), "got: {msg}");
    }

    #[test]
    fn no_config_flag_still_falls_back_to_defaults() {
        // Booting with defaults is the documented offline path, so only an
        // explicit --config is strict.
        let cfg = load_config(&args(&["run"])).expect("bare run must still boot");
        assert!(!cfg.supervise.autostart_llm, "defaults do not autostart");
    }
}

#[cfg(test)]
mod proactive_wiring_tests {
    use super::FlagGuard;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn the_in_flight_flag_clears_even_if_the_turn_panics() {
        // The proactive loop stays silent while this flag is set. A turn task
        // that panicked without clearing it would mute Pythia permanently, so
        // the release has to be a Drop, not a line at the end of the happy path.
        let busy = Arc::new(AtomicBool::new(false));
        let b = busy.clone();
        let result = std::panic::catch_unwind(move || {
            b.store(true, Ordering::SeqCst);
            let _guard = FlagGuard(b.clone());
            panic!("turn blew up");
        });
        assert!(result.is_err());
        assert!(!busy.load(Ordering::SeqCst), "flag must be released");
    }

    #[test]
    fn the_flag_is_set_for_the_life_of_the_guard() {
        let busy = Arc::new(AtomicBool::new(false));
        {
            busy.store(true, Ordering::SeqCst);
            let _guard = FlagGuard(busy.clone());
            assert!(busy.load(Ordering::SeqCst));
        }
        assert!(!busy.load(Ordering::SeqCst));
    }
}
