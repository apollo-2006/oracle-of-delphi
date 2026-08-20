//! Win32 Platform backend (architecture §3.1), Windows only.
//!
//! Implements the [`Platform`] trait against the Win32 API via `windows-sys`:
//!   * windows:  EnumWindows + GetWindowText + SetForegroundWindow
//!   * processes: Toolhelp32 snapshot + OpenProcess/TerminateProcess
//!   * input:    SendInput with Unicode key events
//!
//! Compiled and run on the Windows target; not built in the Linux CI sandbox.
//! The synthetic keystrokes are tagged via `dwExtraInfo` so the dead-man switch
//! in the higher layer can distinguish self-injected from physical input.

use super::{PalError, Platform};
use oracle_ipc::actd::{ProcInfo, WindowInfo};

use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SetForegroundWindow,
};

/// Magic tag stamped into synthetic input's dwExtraInfo so our own events are
/// recognizable to the physical-input watcher.
const Oracle_INPUT_TAG: usize = 0x4A_41_52_56; // "JARV"

#[derive(Default)]
pub struct WindowsPlatform;

impl WindowsPlatform {
    pub fn new() -> Self {
        WindowsPlatform
    }
}

struct EnumCtx {
    windows: Vec<WindowInfo>,
    foreground: HWND,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let ctx = &mut *(lparam as *mut EnumCtx);
    if IsWindowVisible(hwnd) == 0 {
        return 1;
    }
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return 1; // skip untitled windows
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    ctx.windows.push(WindowInfo {
        id: hwnd as usize as u64,
        title,
        pid,
        focused: hwnd == ctx.foreground,
    });
    1
}

impl Platform for WindowsPlatform {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError> {
        let mut ctx = EnumCtx {
            windows: Vec::new(),
            foreground: unsafe { GetForegroundWindow() },
        };
        unsafe {
            EnumWindows(Some(enum_proc), &mut ctx as *mut _ as LPARAM);
        }
        Ok(ctx.windows)
    }

    fn list_processes(&self) -> Result<Vec<ProcInfo>, PalError> {
        let mut out = Vec::new();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snap.is_null() {
                return Err(PalError::Backend("toolhelp snapshot failed".into()));
            }
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            if Process32FirstW(snap, &mut entry) != 0 {
                loop {
                    let name_end = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    let name = String::from_utf16_lossy(&entry.szExeFile[..name_end]);
                    out.push(ProcInfo {
                        pid: entry.th32ProcessID,
                        name,
                        rss_kb: 0, // Toolhelp doesn't report RSS; PSAPI would.
                    });
                    if Process32NextW(snap, &mut entry) == 0 {
                        break;
                    }
                }
            }
            CloseHandle(snap);
        }
        Ok(out)
    }

    fn focus_window(&self, id: u64) -> Result<(), PalError> {
        let hwnd = id as usize as HWND;
        let ok = unsafe { SetForegroundWindow(hwnd) };
        if ok == 0 {
            return Err(PalError::Backend(
                "SetForegroundWindow refused (foreground lock?)".into(),
            ));
        }
        Ok(())
    }

    fn kill_process(&self, pid: u32) -> Result<(), PalError> {
        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle.is_null() {
                return Err(PalError::NoProcess(pid));
            }
            let ok = TerminateProcess(handle, 1);
            CloseHandle(handle);
            if ok == 0 {
                return Err(PalError::Backend(format!("TerminateProcess({pid}) failed")));
            }
        }
        Ok(())
    }

    fn focused_process_name(&self) -> Result<String, PalError> {
        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd.is_null() {
            return Err(PalError::Backend("no foreground window".into()));
        }
        let mut pid: u32 = 0;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        // Resolve pid → name via the process list.
        let procs = self.list_processes()?;
        procs
            .into_iter()
            .find(|p| p.pid == pid)
            .map(|p| p.name)
            .ok_or(PalError::NoProcess(pid))
    }

    fn type_text(&self, text: &str) -> Result<(), PalError> {
        // Build a down+up Unicode key event per UTF-16 code unit.
        let mut inputs: Vec<INPUT> = Vec::with_capacity(text.len() * 2);
        for unit in text.encode_utf16() {
            for &keyup in &[0u32, KEYEVENTF_KEYUP] {
                let mut input: INPUT = unsafe { std::mem::zeroed() };
                input.r#type = INPUT_KEYBOARD;
                input.Anonymous.ki = KEYBDINPUT {
                    wVk: 0,
                    wScan: unit,
                    dwFlags: KEYEVENTF_UNICODE | keyup,
                    time: 0,
                    dwExtraInfo: Oracle_INPUT_TAG,
                };
                inputs.push(input);
            }
        }
        if inputs.is_empty() {
            return Ok(());
        }
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                std::mem::size_of::<INPUT>() as i32,
            )
        };
        if (sent as usize) != inputs.len() {
            return Err(PalError::Backend("SendInput was partially blocked".into()));
        }
        Ok(())
    }
}
