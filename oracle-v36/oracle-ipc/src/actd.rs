//! RPC contract between `oracle-core` and the privileged actuator daemon.
//!
//! The security model lives here in the type system: every request carries a
//! declared [`Capability`], and the daemon's policy engine — not the caller —
//! decides whether to honor it. `serde` `deny_unknown_fields` keeps the wire
//! format strict.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Privilege tiers, enforced inside actd (see architecture §3.4). The model
/// never sees T3 tools at all; T2 requires spoken confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// T0: observe only (window list, process list, RO shell).
    Observe,
    /// T1: benign actuation (focus/resize, media keys, T1 shell).
    BenignAct,
    /// T2: sensitive (kill process, T2 shell, input injection into arbitrary apps).
    Sensitive,
}

/// A hardware media/volume key to tap. Maps to virtual-key presses on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKey {
    PlayPause,
    Next,
    Previous,
    Stop,
    VolumeUp,
    VolumeDown,
    Mute,
}

/// What to do to a window (via Win32 ShowWindow / WM_CLOSE).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowOp {
    Minimize,
    Maximize,
    Restore,
    Close,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActRequest {
    ListWindows,
    ListProcesses,
    FocusWindow {
        window_id: u64,
    },
    KillProcess {
        pid: u32,
    },
    /// Inject a key sequence into the focused window (T2). Scan-code names.
    TypeText {
        text: String,
    },
    /// Open an application, URL, file, or folder through the OS "open" verb
    /// (Windows ShellExecute / Linux xdg-open). Benign and reversible.
    OpenTarget {
        target: String,
    },
    /// Tap a hardware media/volume key (play-pause, next/prev, volume up/down,
    /// mute). Benign and reversible.
    MediaKey {
        key: MediaKey,
    },
    /// Minimize/maximize/restore/close a window by id. Benign and reversible.
    WindowOp {
        window_id: u64,
        action: WindowOp,
    },
    /// Lock the workstation (Win+L). Benign — unlocks with the user's password.
    LockScreen,
    /// Read the UI Automation accessibility tree of a window — the structured
    /// list of on-screen controls (buttons, fields, text) with their names,
    /// values and screen positions. `window_id` None → the foreground window.
    /// Observe-only: it looks, it never touches.
    ReadUiTree {
        #[serde(default)]
        window_id: Option<u64>,
        /// How deep to walk the control tree (daemon clamps to a sane max).
        #[serde(default)]
        max_depth: Option<u32>,
    },
    /// Invoke — a synthetic click / default action — on the first UI element
    /// whose name matches, within a window (`window_id` None → foreground).
    /// Sensitive: a click can trigger anything the app exposes, so it is gated
    /// exactly like input injection.
    InvokeElement {
        #[serde(default)]
        window_id: Option<u64>,
        /// Case-insensitive substring of the element's accessible name.
        name: String,
        /// Optional control-type filter ("button", "menuitem", "checkbox", …).
        #[serde(default)]
        control_type: Option<String>,
    },
    /// Run a command in the PTY sandbox at the given tier.
    ShellExec {
        cmd: String,
        tier: ShellTier,
        timeout_ms: u64,
    },
    /// Enter/exit lockdown — disables injection + shell until re-armed.
    SetLockdown {
        active: bool,
    },
    /// Resolve a previously-parked confirmation (from a `needs_confirmation`
    /// response). Routed straight to the daemon's confirmation handler.
    Confirm {
        request_id: Uuid,
        allow: bool,
    },
}

impl ActRequest {
    /// The capability this request *demands*. The daemon checks its granted
    /// set against this; a caller cannot under-declare to sneak past policy
    /// because the daemon recomputes it from the op, ignoring any client hint.
    pub fn required_capability(&self) -> Capability {
        match self {
            ActRequest::ListWindows
            | ActRequest::ListProcesses
            | ActRequest::ReadUiTree { .. } => Capability::Observe,
            ActRequest::FocusWindow { .. }
            | ActRequest::SetLockdown { .. }
            | ActRequest::OpenTarget { .. }
            | ActRequest::MediaKey { .. }
            | ActRequest::WindowOp { .. }
            | ActRequest::LockScreen => Capability::BenignAct,
            ActRequest::ShellExec { tier, .. } => match tier {
                ShellTier::ReadOnly => Capability::Observe,
                ShellTier::WorkspaceWrite => Capability::BenignAct,
                ShellTier::FullUser => Capability::Sensitive,
            },
            ActRequest::KillProcess { .. }
            | ActRequest::TypeText { .. }
            | ActRequest::InvokeElement { .. } => Capability::Sensitive,
            // Confirm is special-cased before the capability check in the daemon;
            // it carries no capability of its own.
            ActRequest::Confirm { .. } => Capability::Observe,
        }
    }

    /// Whether this op mutates state irreversibly enough to warrant confirmation.
    pub fn is_irreversible(&self) -> bool {
        matches!(
            self,
            ActRequest::KillProcess { .. }
                | ActRequest::ShellExec {
                    tier: ShellTier::FullUser,
                    ..
                }
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellTier {
    ReadOnly,
    WorkspaceWrite,
    FullUser,
}

/// Envelope: every RPC carries a turn id (audit correlation) and a monotonic
/// nonce (anti-replay across the socket).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActEnvelope {
    pub turn_id: Uuid,
    pub nonce: u64,
    pub request: ActRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ActResponse {
    Ok {
        data: serde_json::Value,
    },
    /// Streamed output chunk (shell). `eof` marks the final chunk.
    Chunk {
        stream: StdStream,
        data: String,
        eof: bool,
    },
    Denied {
        reason: String,
    },
    Error {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StdStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WindowInfo {
    pub id: u64,
    pub title: String,
    pub pid: u32,
    pub focused: bool,
    /// Minimized to the taskbar. Such a window is real (keep it, so it can be
    /// focused/restored) but it is NOT what the user is looking at — screen
    /// reads must skip it. Defaults false for backends that can't tell.
    #[serde(default)]
    pub minimized: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub rss_kb: u64,
}

/// One node in a window's UI Automation tree — a control the assistant can see
/// and, if it exposes an action, click by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UiElement {
    /// Depth in the tree (0 = the window root).
    pub depth: u32,
    /// UIA control type, lowercased: button, edit, text, menuitem, checkbox, …
    pub control_type: String,
    /// The element's accessible name (may be empty for anonymous containers).
    pub name: String,
    /// The element's value (edit/combobox contents), when it exposes one.
    pub value: Option<String>,
    /// Whether the element is currently enabled (clickable).
    pub enabled: bool,
    /// Bounding rectangle in screen pixels: [x, y, width, height].
    pub rect: Option<[i32; 4]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_is_derived_from_op_not_trusted() {
        assert_eq!(
            ActRequest::KillProcess { pid: 1 }.required_capability(),
            Capability::Sensitive
        );
        assert_eq!(
            ActRequest::ListWindows.required_capability(),
            Capability::Observe
        );
        assert_eq!(
            ActRequest::ShellExec {
                cmd: "ls".into(),
                tier: ShellTier::ReadOnly,
                timeout_ms: 1000
            }
            .required_capability(),
            Capability::Observe
        );
    }

    #[test]
    fn ui_tree_is_observe_click_is_sensitive() {
        assert_eq!(
            ActRequest::ReadUiTree {
                window_id: None,
                max_depth: None,
            }
            .required_capability(),
            Capability::Observe
        );
        assert_eq!(
            ActRequest::InvokeElement {
                window_id: None,
                name: "Send".into(),
                control_type: None,
            }
            .required_capability(),
            Capability::Sensitive
        );
        // A click is sensitive but reversible-in-principle → not auto-irreversible.
        assert!(!ActRequest::InvokeElement {
            window_id: None,
            name: "Send".into(),
            control_type: None,
        }
        .is_irreversible());
    }

    #[test]
    fn tier_ordering_holds() {
        assert!(Capability::Observe < Capability::Sensitive);
        assert!(Capability::BenignAct < Capability::Sensitive);
    }

    #[test]
    fn irreversible_flags() {
        assert!(ActRequest::KillProcess { pid: 9 }.is_irreversible());
        assert!(!ActRequest::ListWindows.is_irreversible());
    }
}
