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

mod uia;

use super::{PalError, Platform};
use oracle_ipc::actd::{MediaKey, ProcInfo, UiElement, WindowInfo, WindowOp};
use windows_sys::Win32::Foundation::{CloseHandle, BOOL, HWND, LPARAM, RECT};
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE,
};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, PostMessageW, SetForegroundWindow,
    ShowWindow, GWL_EXSTYLE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOWNORMAL, WM_CLOSE,
    WS_EX_TOOLWINDOW,
};

// Virtual-key codes for the media/volume hardware keys (winuser.h).
const VK_VOLUME_MUTE: u16 = 0xAD;
const VK_VOLUME_DOWN: u16 = 0xAE;
const VK_VOLUME_UP: u16 = 0xAF;
const VK_MEDIA_NEXT_TRACK: u16 = 0xB0;
const VK_MEDIA_PREV_TRACK: u16 = 0xB1;
const VK_MEDIA_STOP: u16 = 0xB2;
const VK_MEDIA_PLAY_PAUSE: u16 = 0xB3;

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
    // Skip tool windows — palettes, tray helpers, IME popups. Never user content
    // and never in the alt-tab list, so never what "read my screen" means.
    let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
    if ex_style & WS_EX_TOOLWINDOW != 0 {
        return 1;
    }
    let mut buf = [0u16; 512];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return 1; // skip untitled windows
    }
    let title = String::from_utf16_lossy(&buf[..len as usize]);
    let minimized = IsIconic(hwnd) != 0;
    // For non-minimized windows, drop "ghosts" that Windows still reports as
    // visible: DWM-cloaked windows (a background launcher like Raycast keeps one
    // alive off-screen) and zero/negative-size windows. A minimized window is
    // real (kept so it can be focused/restored) — it just carries the flag.
    if !minimized {
        if is_cloaked(hwnd) {
            return 1;
        }
        let mut r: RECT = std::mem::zeroed();
        if GetWindowRect(hwnd, &mut r) != 0 && (r.right - r.left <= 1 || r.bottom - r.top <= 1) {
            return 1;
        }
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, &mut pid);
    ctx.windows.push(WindowInfo {
        id: hwnd as usize as u64,
        title,
        pid,
        focused: hwnd == ctx.foreground,
        minimized,
    });
    1
}

/// True if the window is DWM-cloaked — composited-but-hidden (background modern
/// apps, virtual-desktop-parked windows). These pass `IsWindowVisible` yet aren't
/// on screen, which is exactly how a dismissed launcher leaks into a screen read.
unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked: u32 = 0;
    let hr = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED as u32,
        &mut cloaked as *mut u32 as *mut core::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
    hr == 0 && cloaked != 0
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

    fn open_target(&self, target: &str) -> Result<(), PalError> {
        // The universal "open" verb: launches apps on PATH, opens URLs in the
        // default browser, and opens files/folders with their default handler.
        let verb = wide("open");
        let file = wide(target);
        let ret = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL as i32,
            )
        };
        // ShellExecuteW returns a value > 32 on success (legacy HINSTANCE ABI).
        if (ret as isize) <= 32 {
            return Err(PalError::Backend(format!(
                "ShellExecute could not open '{target}' (code {})",
                ret as isize
            )));
        }
        Ok(())
    }

    fn window_op(&self, id: u64, op: WindowOp) -> Result<(), PalError> {
        let hwnd = id as usize as HWND;
        unsafe {
            match op {
                WindowOp::Minimize => {
                    ShowWindow(hwnd, SW_MINIMIZE);
                }
                WindowOp::Maximize => {
                    ShowWindow(hwnd, SW_MAXIMIZE);
                }
                WindowOp::Restore => {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                WindowOp::Close => {
                    // WM_CLOSE asks the window to close (apps can still prompt to
                    // save) — gentler than terminating the process.
                    if PostMessageW(hwnd, WM_CLOSE, 0, 0) == 0 {
                        return Err(PalError::Backend("PostMessage(WM_CLOSE) failed".into()));
                    }
                }
            }
        }
        Ok(())
    }

    fn lock_screen(&self) -> Result<(), PalError> {
        // rundll32 invokes user32!LockWorkStation without a Win32 binding.
        use std::os::windows::process::CommandExt;
        std::process::Command::new("rundll32.exe")
            .args(["user32.dll,LockWorkStation"])
            .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
            .spawn()
            .map(|_| ())
            .map_err(|e| PalError::Backend(format!("lock: {e}")))
    }

    fn media_key(&self, key: MediaKey) -> Result<(), PalError> {
        // Volume steps are small (~2% each); tap several times for a noticeable
        // change. Transport keys fire once.
        let (vk, times) = match key {
            MediaKey::PlayPause => (VK_MEDIA_PLAY_PAUSE, 1),
            MediaKey::Next => (VK_MEDIA_NEXT_TRACK, 1),
            MediaKey::Previous => (VK_MEDIA_PREV_TRACK, 1),
            MediaKey::Stop => (VK_MEDIA_STOP, 1),
            MediaKey::VolumeUp => (VK_VOLUME_UP, 4),
            MediaKey::VolumeDown => (VK_VOLUME_DOWN, 4),
            MediaKey::Mute => (VK_VOLUME_MUTE, 1),
        };
        tap_vk(vk, times)
    }

    fn read_ui_tree(
        &self,
        window_id: Option<u64>,
        max_depth: u32,
    ) -> Result<Vec<UiElement>, PalError> {
        uia::read_ui_tree(window_id, max_depth)
    }

    fn invoke_element(
        &self,
        window_id: Option<u64>,
        name: &str,
        control_type: Option<&str>,
    ) -> Result<UiElement, PalError> {
        uia::invoke_element(window_id, name, control_type)
    }
}

/// A NUL-terminated UTF-16 buffer for the wide Win32 APIs.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Press and release a virtual key `times` times via SendInput.
fn tap_vk(vk: u16, times: u32) -> Result<(), PalError> {
    let mut inputs: Vec<INPUT> = Vec::with_capacity((times * 2) as usize);
    for _ in 0..times {
        for &keyup in &[0u32, KEYEVENTF_KEYUP] {
            let mut input: INPUT = unsafe { std::mem::zeroed() };
            input.r#type = INPUT_KEYBOARD;
            input.Anonymous.ki = KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: keyup,
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
        return Err(PalError::Backend(
            "SendInput (media key) was blocked".into(),
        ));
    }
    Ok(())
}
