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
use oracle_ipc::actd::{ActRequest, ActResponse, ShellTier};
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

/// Register the OS-control tools.
pub fn register_all(reg: &mut super::ToolRegistry) {
    reg.register(ListWindows)
        .register(ListProcesses)
        .register(FocusWindow)
        .register(Shell)
        .register(TypeText)
        .register(KillProcess);
}
