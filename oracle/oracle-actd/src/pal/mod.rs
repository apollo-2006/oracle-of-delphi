//! Platform Abstraction Layer (architecture §3.1).
//!
//! One trait, multiple OS backends. The real Linux backend uses x11rb/EWMH +
//! `/dev/uinput`; the Windows backend uses `EnumWindows`/`SendInput`. To keep
//! the reference build hermetic and cross-platform-testable, this ships a
//! `MockPlatform` that models the same semantics in memory. The daemon is
//! generic over [`Platform`], so the same policy/RPC/audit code runs on any
//! backend.

use oracle_ipc::actd::{ProcInfo, WindowInfo};

pub mod mock;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(windows)]
pub mod windows;

pub use mock::MockPlatform;

#[derive(Debug, thiserror::Error)]
pub enum PalError {
    #[error("no such window: {0}")]
    NoWindow(u64),
    #[error("no such process: {0}")]
    NoProcess(u32),
    #[error("injection blocked: {0}")]
    InjectionBlocked(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// The OS operations the daemon needs. Kept small and synchronous; the RPC
/// layer wraps these with async + streaming where needed (shell).
pub trait Platform: Send + Sync {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError>;
    fn list_processes(&self) -> Result<Vec<ProcInfo>, PalError>;
    fn focus_window(&self, id: u64) -> Result<(), PalError>;
    fn kill_process(&self, pid: u32) -> Result<(), PalError>;
    /// Inject text into the focused window. Returns the process name of the
    /// target so the policy layer can double-check the denylist post-focus.
    fn focused_process_name(&self) -> Result<String, PalError>;
    fn type_text(&self, text: &str) -> Result<(), PalError>;
}
