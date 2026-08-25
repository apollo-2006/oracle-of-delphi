//! OS-control tools (architecture §3), exposed to the agent and executed by the
//! privileged actuator daemon over the authenticated socket.
//!
//! Safety model (§3.4): the daemon — not these tools — decides what is allowed.
//! Observe/benign ops run directly; sensitive ops require the daemon's Sensitive
//! grant (`oracle-actd --serve … --grant-sensitive`); irreversible ops (kill,
//! full-user shell) are parked by the daemon and routed through the user's
//! confirmer — the Apollo decree modal in the HUD, or a y/N prompt in the REPL.
//! Only on the user's explicit sanction does the tool send the `Confirm` RPC
//! that actually executes. So the agent can never silently do anything
//! irreversible; a human passes sentence first.

use super::{ToolCtx, ToolError, ToolErrorKind, ToolOutcome, TypedTool};
use oracle_ipc::actd::{ActRequest, ActResponse, MediaKey, ShellTier, WindowOp};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;

/// Send a request to actd and normalize the response. If the daemon parks the
/// action for confirmation, ask the user (via the [`Confirmer`]) and, on
/// approval, send the `Confirm` RPC that actually executes it. `action` is a
/// human-readable description used for the confirmation prompt.
///
/// [`Confirmer`]: crate::confirm::Confirmer
async fn call_actd(ctx: &ToolCtx, req: ActRequest, action: &str) -> ToolOutcome {
    let Some(client) = ctx.shared.actd.clone() else {
        return ToolOutcome::Err(ToolError {
            status: ToolErrorKind::Denied,
            field: None,
            reason: "the actuator daemon (oracle-actd) is not connected".into(),
            hint: Some(
                "start it with `oracle-actd --serve <socket-or-pipe>` and set [actd] socket in the config"
                    .into(),
            ),
        });
    };
    match client.call(ctx.turn_id, req).await {
        Ok(ActResponse::Ok { data }) => {
            // The daemon parks irreversible actions and asks for confirmation.
            if data.get("needs_confirmation").and_then(|v| v.as_bool()) == Some(true) {
                let request_id = data
                    .get("request_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| uuid::Uuid::parse_str(s).ok());
                let Some(request_id) = request_id else {
                    return ToolOutcome::Err(ToolError::transient(
                        "daemon requested confirmation but returned no request id",
                    ));
                };
                // Ask the user for their decree.
                let approved = ctx.shared.confirmer.request(action, "irreversible").await;
                // Resolve the parked action either way (allow=false discards it).
                match client
                    .call(
                        ctx.turn_id,
                        ActRequest::Confirm {
                            request_id,
                            allow: approved,
                        },
                    )
                    .await
                {
                    Ok(ActResponse::Ok { data }) => ToolOutcome::Ok(data),
                    Ok(ActResponse::Denied { reason }) => ToolOutcome::Err(ToolError {
                        status: ToolErrorKind::Denied,
                        field: None,
                        reason: format!("declined: {reason}"),
                        hint: None,
                    }),
                    Ok(ActResponse::Error { reason }) => {
                        ToolOutcome::Err(ToolError::transient(&reason))
                    }
                    Ok(ActResponse::Chunk { data, .. }) => {
                        ToolOutcome::Ok(json!({ "output": data }))
                    }
                    Err(e) => ToolOutcome::Err(ToolError::transient(&format!("actd confirm: {e}"))),
                }
            } else {
                ToolOutcome::Ok(data)
            }
        }
        Ok(ActResponse::Chunk { data, .. }) => ToolOutcome::Ok(json!({ "output": data })),
        Ok(ActResponse::Denied { reason }) => ToolOutcome::Err(ToolError {
            status: ToolErrorKind::Denied,
            field: None,
            reason,
            hint: None,
        }),
        Ok(ActResponse::Error { reason }) => ToolOutcome::Err(ToolError::transient(&reason)),
        Err(e) => ToolOutcome::Err(ToolError::transient(&format!("actd rpc: {e}"))),
    }
}

// --- Observe ------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct NoArgs {}

pub struct ListWindows;
#[async_trait::async_trait]
impl TypedTool for ListWindows {
    type Args = NoArgs;
    const NAME: &'static str = "os.list_windows";
    const DESCRIPTION: &'static str =
        "List the open windows on the user's desktop (title, pid, focused).";
    async fn run(&self, _a: NoArgs, ctx: &ToolCtx) -> ToolOutcome {
        call_actd(ctx, ActRequest::ListWindows, "list open windows").await
    }
}

pub struct ListProcesses;
#[async_trait::async_trait]
impl TypedTool for ListProcesses {
    type Args = NoArgs;
    const NAME: &'static str = "os.list_processes";
    const DESCRIPTION: &'static str = "List running processes (pid, name, memory).";
    async fn run(&self, _a: NoArgs, ctx: &ToolCtx) -> ToolOutcome {
        call_actd(ctx, ActRequest::ListProcesses, "list running processes").await
    }
}

// --- Benign act ---------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct FocusArgs {
    /// The window id from os.list_windows.
    pub window_id: u64,
}
pub struct FocusWindow;
#[async_trait::async_trait]
impl TypedTool for FocusWindow {
    type Args = FocusArgs;
    const NAME: &'static str = "os.focus_window";
    const DESCRIPTION: &'static str = "Bring a window to the foreground by its id.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: FocusArgs, ctx: &ToolCtx) -> ToolOutcome {
        let action = format!("focus window {}", a.window_id);
        call_actd(
            ctx,
            ActRequest::FocusWindow {
                window_id: a.window_id,
            },
            &action,
        )
        .await
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ShellArgs {
    /// The command line to run. Read-only commands run directly; risky ones
    /// (destructive, network-fetch, privilege-escalation) require confirmation.
    pub cmd: String,
    /// Timeout in milliseconds (default 30000).
    #[serde(default = "default_shell_timeout")]
    pub timeout_ms: u64,
}
fn default_shell_timeout() -> u64 {
    30_000
}
pub struct Shell;
#[async_trait::async_trait]
impl TypedTool for Shell {
    type Args = ShellArgs;
    const NAME: &'static str = "os.shell";
    const DESCRIPTION: &'static str =
        "Run a shell command. Read-only commands run directly; the daemon classifies and gates risky ones.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: ShellArgs, ctx: &ToolCtx) -> ToolOutcome {
        // The daemon re-classifies the command and picks the real tier; we send
        // ReadOnly as the *requested* tier and let it upgrade as needed.
        let action = format!("run shell command: {}", a.cmd);
        call_actd(
            ctx,
            ActRequest::ShellExec {
                cmd: a.cmd,
                tier: ShellTier::ReadOnly,
                timeout_ms: a.timeout_ms,
            },
            &action,
        )
        .await
    }
}

// --- Sensitive ----------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct TypeTextArgs {
    /// Text to type into the focused window.
    pub text: String,
}
pub struct TypeText;
#[async_trait::async_trait]
impl TypedTool for TypeText {
    type Args = TypeTextArgs;
    const NAME: &'static str = "os.type_text";
    const DESCRIPTION: &'static str =
        "Type text into the currently focused window (requires the daemon's sensitive grant; refused for password fields).";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: TypeTextArgs, ctx: &ToolCtx) -> ToolOutcome {
        {
            let action = format!("type text into the focused window ({} chars)", a.text.len());
            call_actd(ctx, ActRequest::TypeText { text: a.text }, &action).await
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct KillArgs {
    /// The pid to terminate (from os.list_processes).
    pub pid: u32,
}
pub struct KillProcess;
#[async_trait::async_trait]
impl TypedTool for KillProcess {
    type Args = KillArgs;
    const NAME: &'static str = "os.kill_process";
    const DESCRIPTION: &'static str =
        "Terminate a process by pid. Irreversible — requires user confirmation.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Irreversible;
    async fn run(&self, a: KillArgs, ctx: &ToolCtx) -> ToolOutcome {
        {
            let action = format!("terminate process pid {}", a.pid);
            call_actd(ctx, ActRequest::KillProcess { pid: a.pid }, &action).await
        }
    }
}

// --- Launch / open / search (benign act) --------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct LaunchArgs {
    /// The application to launch — a name Windows can resolve (e.g. "notepad",
    /// "spotify", "chrome") or a full path to an .exe.
    pub name: String,
}
pub struct LaunchApp;
#[async_trait::async_trait]
impl TypedTool for LaunchApp {
    type Args = LaunchArgs;
    const NAME: &'static str = "os.launch_app";
    const DESCRIPTION: &'static str =
        "Launch an application by name (e.g. 'spotify', 'notepad', 'chrome') or full path.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: LaunchArgs, ctx: &ToolCtx) -> ToolOutcome {
        let action = format!("launch {}", a.name);
        call_actd(ctx, ActRequest::OpenTarget { target: a.name }, &action).await
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct OpenUrlArgs {
    /// The URL to open in the default browser (include the scheme, e.g. https://).
    pub url: String,
}
pub struct OpenUrl;
#[async_trait::async_trait]
impl TypedTool for OpenUrl {
    type Args = OpenUrlArgs;
    const NAME: &'static str = "os.open_url";
    const DESCRIPTION: &'static str = "Open a URL in the user's default browser.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: OpenUrlArgs, ctx: &ToolCtx) -> ToolOutcome {
        let action = format!("open {}", a.url);
        call_actd(ctx, ActRequest::OpenTarget { target: a.url }, &action).await
    }
    fn validate(a: &OpenUrlArgs) -> Result<(), ToolError> {
        if !(a.url.starts_with("http://") || a.url.starts_with("https://")) {
            return Err(ToolError::invalid(
                "url",
                "missing http(s) scheme",
                "include https:// at the front",
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WebSearchArgs {
    /// What to search the web for.
    pub query: String,
}
pub struct WebSearch;
#[async_trait::async_trait]
impl TypedTool for WebSearch {
    type Args = WebSearchArgs;
    const NAME: &'static str = "os.web_search";
    const DESCRIPTION: &'static str =
        "Search the web for a query by opening the results in the user's default browser.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: WebSearchArgs, ctx: &ToolCtx) -> ToolOutcome {
        let url = format!("https://www.google.com/search?q={}", url_encode(&a.query));
        let action = format!("web-search '{}'", a.query);
        call_actd(ctx, ActRequest::OpenTarget { target: url }, &action).await
    }
}

/// Minimal query-string percent-encoding: keep unreserved characters, encode
/// everything else (spaces become %20). Enough for a search URL.
fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// --- Media / volume (benign act) ----------------------------------------

#[derive(Deserialize, JsonSchema)]
pub struct MediaArgs {
    /// One of: play_pause, next, previous, stop, volume_up, volume_down, mute.
    pub action: String,
}
pub struct Media;
#[async_trait::async_trait]
impl TypedTool for Media {
    type Args = MediaArgs;
    const NAME: &'static str = "os.media";
    const DESCRIPTION: &'static str =
        "Control media & volume: play_pause, next, previous, stop, volume_up, volume_down, mute. \
         play_pause TOGGLES playback — to pause or to resume, call it exactly once (calling twice \
         undoes itself). State can't be read back, so report the action, not the resulting state.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: MediaArgs, ctx: &ToolCtx) -> ToolOutcome {
        let Some(key) = parse_media_key(&a.action) else {
            return ToolOutcome::Err(ToolError::invalid(
                "action",
                &format!("unknown media action '{}'", a.action),
                "use play_pause, next, previous, stop, volume_up, volume_down, or mute",
            ));
        };
        let action = format!("media key {}", a.action);
        call_actd(ctx, ActRequest::MediaKey { key }, &action).await
    }
}

/// Map a forgiving set of spoken/typed synonyms to a [`MediaKey`].
fn parse_media_key(s: &str) -> Option<MediaKey> {
    match s.trim().to_lowercase().replace([' ', '-'], "_").as_str() {
        "play_pause" | "playpause" | "play" | "pause" | "toggle" => Some(MediaKey::PlayPause),
        "next" | "next_track" | "skip" | "forward" => Some(MediaKey::Next),
        "previous" | "prev" | "prev_track" | "back" => Some(MediaKey::Previous),
        "stop" => Some(MediaKey::Stop),
        "volume_up" | "vol_up" | "louder" | "up" => Some(MediaKey::VolumeUp),
        "volume_down" | "vol_down" | "quieter" | "down" => Some(MediaKey::VolumeDown),
        "mute" | "unmute" | "silence" => Some(MediaKey::Mute),
        _ => None,
    }
}

// --- Focus / window control by name (benign act) -------------------------

/// Resolve a window by a case-insensitive title substring: returns its (id,
/// title) or a ready ToolOutcome error explaining the miss.
async fn find_window(ctx: &ToolCtx, query: &str) -> Result<(u64, String), ToolOutcome> {
    let listed = call_actd(ctx, ActRequest::ListWindows, "list windows").await;
    let data = match listed {
        ToolOutcome::Ok(d) => d,
        other => return Err(other),
    };
    let q = query.trim().to_lowercase();
    let Some(windows) = data.get("windows").and_then(|w| w.as_array()) else {
        return Err(ToolOutcome::Err(ToolError::transient(
            "actd returned no window list",
        )));
    };

    let win_tuple = |w: &serde_json::Value| -> Option<(u64, String)> {
        let id = w.get("id").and_then(|v| v.as_u64())?;
        let title = w
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string();
        Some((id, title))
    };

    // 1) Direct window-title match — the common case.
    if let Some(hit) = windows
        .iter()
        .find(|w| {
            w.get("title")
                .and_then(|t| t.as_str())
                .map(|t| t.to_lowercase().contains(&q))
                .unwrap_or(false)
        })
        .and_then(win_tuple)
    {
        return Ok(hit);
    }

    // 2) Process-name match. Many apps rewrite their title to the current
    //    document/track — Spotify becomes "Artist – Song" while playing, so
    //    "spotify" never matches the title. The process is still "Spotify.exe",
    //    so match the owning process name too.
    if let ToolOutcome::Ok(pdata) = call_actd(ctx, ActRequest::ListProcesses, "list processes").await
    {
        if let Some(procs) = pdata.get("processes").and_then(|p| p.as_array()) {
            let name_of = |pid: u64| -> String {
                procs
                    .iter()
                    .find(|p| p.get("pid").and_then(|v| v.as_u64()) == Some(pid))
                    .and_then(|p| p.get("name").and_then(|n| n.as_str()))
                    .unwrap_or_default()
                    .to_lowercase()
            };
            if let Some(hit) = windows
                .iter()
                .find(|w| {
                    w.get("pid")
                        .and_then(|v| v.as_u64())
                        .map(|pid| name_of(pid).contains(&q))
                        .unwrap_or(false)
                })
                .and_then(win_tuple)
            {
                return Ok(hit);
            }
        }
    }

    // 3) Miss — list a few real titles so the model (and user) can see what's
    //    actually open and retry with a correct one in a single step.
    let titles: Vec<String> = windows
        .iter()
        .filter_map(|w| w.get("title").and_then(|t| t.as_str()))
        .filter(|t| !t.trim().is_empty())
        .take(8)
        .map(|t| t.to_string())
        .collect();
    Err(ToolOutcome::Err(ToolError::invalid(
        "query",
        &format!(
            "no open window matches '{query}'. Open windows: {}",
            if titles.is_empty() {
                "(none)".to_string()
            } else {
                titles.join(", ")
            }
        ),
        "use one of those titles, or the app's name",
    )))
}

#[derive(Deserialize, JsonSchema)]
pub struct FocusAppArgs {
    /// Part of the window title to bring to the front (case-insensitive), e.g.
    /// "spotify", "chrome", "code".
    pub query: String,
}
pub struct FocusApp;
#[async_trait::async_trait]
impl TypedTool for FocusApp {
    type Args = FocusAppArgs;
    const NAME: &'static str = "os.focus_app";
    const DESCRIPTION: &'static str =
        "Bring a window to the front by matching part of its title (no need to know window ids).";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: FocusAppArgs, ctx: &ToolCtx) -> ToolOutcome {
        let (id, title) = match find_window(ctx, &a.query).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let action = format!("focus '{title}'");
        call_actd(ctx, ActRequest::FocusWindow { window_id: id }, &action).await
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct WindowArgs {
    /// Part of the window title to act on (case-insensitive), e.g. "spotify".
    pub query: String,
    /// One of: minimize, maximize, restore, close.
    pub action: String,
}
pub struct WindowControl;
#[async_trait::async_trait]
impl TypedTool for WindowControl {
    type Args = WindowArgs;
    const NAME: &'static str = "os.window";
    const DESCRIPTION: &'static str =
        "Minimize, maximize, restore, or close a window by matching its title. action = minimize | maximize | restore | close.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: WindowArgs, ctx: &ToolCtx) -> ToolOutcome {
        let Some(op) = parse_window_op(&a.action) else {
            return ToolOutcome::Err(ToolError::invalid(
                "action",
                &format!("unknown window action '{}'", a.action),
                "use minimize, maximize, restore, or close",
            ));
        };
        let (id, title) = match find_window(ctx, &a.query).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let action = format!("{} '{title}'", a.action);
        call_actd(
            ctx,
            ActRequest::WindowOp {
                window_id: id,
                action: op,
            },
            &action,
        )
        .await
    }
}

fn parse_window_op(s: &str) -> Option<WindowOp> {
    match s.trim().to_lowercase().as_str() {
        "minimize" | "minimise" | "min" | "hide" => Some(WindowOp::Minimize),
        "maximize" | "maximise" | "max" | "fullscreen" => Some(WindowOp::Maximize),
        "restore" | "unminimize" | "normal" => Some(WindowOp::Restore),
        "close" | "quit" | "exit" => Some(WindowOp::Close),
        _ => None,
    }
}

pub struct LockScreen;
#[async_trait::async_trait]
impl TypedTool for LockScreen {
    type Args = NoArgs;
    const NAME: &'static str = "os.lock_screen";
    const DESCRIPTION: &'static str =
        "Lock the computer (the user's password is needed to unlock).";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, _a: NoArgs, ctx: &ToolCtx) -> ToolOutcome {
        call_actd(ctx, ActRequest::LockScreen, "lock the screen").await
    }
}

// --- Screen awareness: UI Automation tree (observe) + click (sensitive) ----

/// Resolve an optional window-title query to a concrete window id. Empty/None
/// means "the foreground window" (`None`); a non-empty query is matched against
/// open window titles.
async fn resolve_window(
    ctx: &ToolCtx,
    query: &Option<String>,
) -> Result<Option<u64>, ToolOutcome> {
    match query {
        Some(q) if !q.trim().is_empty() => {
            let (id, _title) = find_window(ctx, q).await?;
            Ok(Some(id))
        }
        _ => Ok(None),
    }
}

/// The window the user is actually looking at, for a no-argument screen read.
///
/// The catch: when the user talks to Pythia through the HUD, the *foreground*
/// window is the Oracle itself — so a naive "read the foreground window" reads
/// Pythia's own UI and the model then hallucinates. Windows are enumerated in
/// z-order (top first), so we skip our own window (and untitled shells) and take
/// the topmost real window behind us — what the user was last looking at.
async fn frontmost_readable_window(ctx: &ToolCtx) -> Option<(u64, String)> {
    let data = match call_actd(ctx, ActRequest::ListWindows, "list windows").await {
        ToolOutcome::Ok(d) => d,
        _ => return None,
    };
    let windows = data.get("windows").and_then(|w| w.as_array())?;
    for w in windows {
        let title = w.get("title").and_then(|t| t.as_str()).unwrap_or_default();
        let low = title.to_lowercase();
        let minimized = w
            .get("minimized")
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        // Skip: our own window, untitled shells, and minimized windows (real, but
        // not what the user is looking at right now).
        if title.trim().is_empty() || low.contains("oracle of delphi") || minimized {
            continue;
        }
        if let Some(id) = w.get("id").and_then(|v| v.as_u64()) {
            return Some((id, title.to_string()));
        }
    }
    None
}

#[derive(Deserialize, JsonSchema)]
pub struct ReadScreenArgs {
    /// Optional: part of a window title to inspect (case-insensitive). Omit to
    /// read whatever window is currently in the foreground.
    #[serde(default)]
    pub window: Option<String>,
    /// Optional: how deep to walk the control tree (default 12).
    #[serde(default)]
    pub max_depth: Option<u32>,
}
pub struct ReadScreen;
#[async_trait::async_trait]
impl TypedTool for ReadScreen {
    type Args = ReadScreenArgs;
    const NAME: &'static str = "os.read_screen";
    const DESCRIPTION: &'static str =
        "See what's on screen: reads a window's accessibility tree (buttons, fields, text, values) \
         — not a screenshot. No 'window' reads the window the user is looking at (never your own). \
         Ground your answer ONLY in the returned 'elements'; if empty, say you couldn't read it — \
         don't guess. Observe-only.";
    async fn run(&self, a: ReadScreenArgs, ctx: &ToolCtx) -> ToolOutcome {
        // Resolve which window to read, and its title (for grounding the model).
        let (window_id, target_title): (Option<u64>, Option<String>) = match &a.window {
            Some(q) if !q.trim().is_empty() => match find_window(ctx, q).await {
                Ok((id, title)) => (Some(id), Some(title)),
                Err(e) => return e,
            },
            _ => match frontmost_readable_window(ctx).await {
                Some((id, title)) => (Some(id), Some(title)),
                None => (None, None), // last resort: whatever's in front
            },
        };
        let action = match &target_title {
            Some(t) => format!("read the screen of '{t}'"),
            None => "read the on-screen UI".to_string(),
        };
        let outcome = call_actd(
            ctx,
            ActRequest::ReadUiTree {
                window_id,
                max_depth: a.max_depth,
            },
            &action,
        )
        .await;
        // Tell the model which window it actually read, so it grounds its answer
        // on that (and doesn't confuse it with its own window).
        match outcome {
            ToolOutcome::Ok(mut data) => {
                if let Some(t) = target_title {
                    if let Some(obj) = data.as_object_mut() {
                        obj.insert("window".into(), json!(t));
                    }
                }
                ToolOutcome::Ok(data)
            }
            other => other,
        }
    }
}

#[derive(Deserialize, JsonSchema)]
pub struct ClickArgs {
    /// The visible name/label of the control to click (case-insensitive
    /// substring), e.g. "Save", "Sign in", "OK".
    pub name: String,
    /// Optional: restrict to a control type — "button", "menuitem", "checkbox",
    /// "radiobutton", "tab", "listitem", "hyperlink", etc.
    #[serde(default)]
    pub control_type: Option<String>,
    /// Optional: part of the window title to act within (case-insensitive). Omit
    /// to act on the foreground window.
    #[serde(default)]
    pub window: Option<String>,
}
pub struct Click;
#[async_trait::async_trait]
impl TypedTool for Click {
    type Args = ClickArgs;
    const NAME: &'static str = "os.click";
    const DESCRIPTION: &'static str =
        "Click a UI element by its visible name (button, menu item, checkbox, tab, link). Find the \
         exact name first with os.read_screen. Sensitive — needs the user's confirmation.";
    const SIDE_EFFECT: super::SideEffect = super::SideEffect::Reversible;
    async fn run(&self, a: ClickArgs, ctx: &ToolCtx) -> ToolOutcome {
        let window_id = match resolve_window(ctx, &a.window).await {
            Ok(v) => v,
            Err(e) => return e,
        };
        let action = format!("click '{}'", a.name);
        call_actd(
            ctx,
            ActRequest::InvokeElement {
                window_id,
                name: a.name,
                control_type: a.control_type,
            },
            &action,
        )
        .await
    }
    fn validate(a: &ClickArgs) -> Result<(), ToolError> {
        if a.name.trim().is_empty() {
            return Err(ToolError::invalid(
                "name",
                "no element name given",
                "pass the visible label of the control to click",
            ));
        }
        Ok(())
    }
}

/// Register the OS-control tools.
pub fn register_all(reg: &mut super::ToolRegistry) {
    reg.register(ListWindows)
        .register(ListProcesses)
        .register(FocusWindow)
        .register(FocusApp)
        .register(WindowControl)
        .register(LockScreen)
        .register(LaunchApp)
        .register(OpenUrl)
        .register(WebSearch)
        .register(Media)
        .register(ReadScreen)
        .register(Click)
        .register(Shell)
        .register(TypeText)
        .register(KillProcess);
}

#[cfg(test)]
mod tests {
    use super::*;
    use oracle_ipc::actd::MediaKey;

    #[test]
    fn media_synonyms_map_to_keys() {
        assert_eq!(parse_media_key("play"), Some(MediaKey::PlayPause));
        assert_eq!(parse_media_key("Play Pause"), Some(MediaKey::PlayPause));
        assert_eq!(parse_media_key("skip"), Some(MediaKey::Next));
        assert_eq!(parse_media_key("louder"), Some(MediaKey::VolumeUp));
        assert_eq!(parse_media_key("quieter"), Some(MediaKey::VolumeDown));
        assert_eq!(parse_media_key("mute"), Some(MediaKey::Mute));
        assert_eq!(parse_media_key("frobnicate"), None);
    }

    #[test]
    fn url_encode_escapes_query() {
        assert_eq!(url_encode("rust lang"), "rust%20lang");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
        assert_eq!(url_encode("plain-text_1.0~"), "plain-text_1.0~");
    }

    #[test]
    fn window_op_synonyms() {
        assert_eq!(parse_window_op("minimize"), Some(WindowOp::Minimize));
        assert_eq!(parse_window_op("Max"), Some(WindowOp::Maximize));
        assert_eq!(parse_window_op("close"), Some(WindowOp::Close));
        assert_eq!(parse_window_op("restore"), Some(WindowOp::Restore));
        assert_eq!(parse_window_op("wobble"), None);
    }
}
