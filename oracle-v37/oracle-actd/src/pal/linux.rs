//! Linux Platform backend.
//!
//! Process enumeration is implemented for real against `/proc` (dependency-free
//! and genuinely functional). Window management and input injection require
//! x11rb/EWMH and `/dev/uinput` respectively; those need extra crates and a
//! live session, so here they return an honest `Backend` error describing what
//! the production build wires in. This mirrors the architecture's insistence on
//! *reporting degraded capabilities honestly* (§3.1) rather than pretending.

use super::{PalError, Platform};
use oracle_ipc::actd::{ProcInfo, UiElement, WindowInfo};
use std::fs;

#[derive(Default)]
pub struct LinuxPlatform;

impl LinuxPlatform {
    pub fn new() -> Self {
        LinuxPlatform
    }
}

impl Platform for LinuxPlatform {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError> {
        // Production: x11rb EWMH (_NET_CLIENT_LIST) / wlr-foreign-toplevel.
        Err(PalError::Backend(
            "window enumeration requires the X11/Wayland backend (x11rb / wlr protocols)".into(),
        ))
    }

    fn list_processes(&self) -> Result<Vec<ProcInfo>, PalError> {
        let mut out = Vec::new();
        let entries =
            fs::read_dir("/proc").map_err(|e| PalError::Backend(format!("/proc: {e}")))?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            let comm = fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|s| s.trim().to_string())
                .unwrap_or_default();
            let rss_kb = read_rss_kb(pid).unwrap_or(0);
            out.push(ProcInfo {
                pid,
                name: comm,
                rss_kb,
            });
        }
        Ok(out)
    }

    fn focus_window(&self, _id: u64) -> Result<(), PalError> {
        Err(PalError::Backend(
            "focus requires the X11/Wayland backend".into(),
        ))
    }

    fn kill_process(&self, pid: u32) -> Result<(), PalError> {
        // Production uses pidfd_open + pidfd_send_signal (race-free). A plain
        // kill(2) via libc is the portable fallback; we avoid an unsafe libc
        // dependency in the reference build and surface the intent.
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            return Err(PalError::NoProcess(pid));
        }
        Err(PalError::Backend(
            "kill wired via pidfd_send_signal in production build".into(),
        ))
    }

    fn focused_process_name(&self) -> Result<String, PalError> {
        Err(PalError::Backend(
            "focused window query requires the X11/Wayland backend".into(),
        ))
    }

    fn type_text(&self, _text: &str) -> Result<(), PalError> {
        Err(PalError::Backend(
            "input injection requires /dev/uinput access".into(),
        ))
    }

    fn open_target(&self, target: &str) -> Result<(), PalError> {
        // xdg-open is the desktop-standard "open" verb (apps, URLs, files).
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| PalError::Backend(format!("xdg-open: {e}")))
    }

    fn media_key(&self, _key: oracle_ipc::actd::MediaKey) -> Result<(), PalError> {
        // Production wires playerctl / uinput; the reference build reports it.
        Err(PalError::Backend(
            "media keys require playerctl or /dev/uinput on Linux".into(),
        ))
    }

    fn window_op(&self, _id: u64, _op: oracle_ipc::actd::WindowOp) -> Result<(), PalError> {
        Err(PalError::Backend(
            "window control requires the X11/Wayland backend".into(),
        ))
    }

    fn lock_screen(&self) -> Result<(), PalError> {
        // Best-effort: loginctl lock-session on systemd desktops.
        std::process::Command::new("loginctl")
            .arg("lock-session")
            .status()
            .map(|_| ())
            .map_err(|e| PalError::Backend(format!("loginctl: {e}")))
    }

    fn read_ui_tree(
        &self,
        _window_id: Option<u64>,
        _max_depth: u32,
    ) -> Result<Vec<UiElement>, PalError> {
        // Production wires the AT-SPI2 accessibility bus; the reference build
        // reports the missing backend rather than pretending.
        Err(PalError::Backend(
            "reading the UI tree requires the AT-SPI accessibility backend on Linux".into(),
        ))
    }

    fn invoke_element(
        &self,
        _window_id: Option<u64>,
        _name: &str,
        _control_type: Option<&str>,
    ) -> Result<UiElement, PalError> {
        Err(PalError::Backend(
            "invoking UI elements requires the AT-SPI accessibility backend on Linux".into(),
        ))
    }
}

/// Parse RSS (in kB) from /proc/<pid>/statm (second field = resident pages).
fn read_rss_kb(pid: u32) -> Option<u64> {
    let statm = fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
    let resident_pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    let page_kb = 4; // 4 KiB pages on typical x86_64
    Some(resident_pages * page_kb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_real_processes() {
        // This test runs on Linux CI: /proc must contain at least our own pid.
        let p = LinuxPlatform::new();
        let procs = p.list_processes().unwrap();
        let me = std::process::id();
        assert!(procs.iter().any(|p| p.pid == me), "should see own pid");
    }

    #[test]
    fn kill_missing_process_is_no_process() {
        let p = LinuxPlatform::new();
        // pid 0 never has a /proc entry
        assert!(matches!(p.kill_process(0), Err(PalError::NoProcess(_))));
    }
}
