//! In-memory Platform backend: models windows/processes/focus/typing so the
//! daemon's policy + RPC + audit paths are fully testable on any OS.

use super::{PalError, Platform};
use oracle_ipc::actd::{ProcInfo, WindowInfo};
use std::sync::Mutex;

pub struct MockPlatform {
    inner: Mutex<Inner>,
}

struct Inner {
    windows: Vec<WindowInfo>,
    processes: Vec<ProcInfo>,
    typed: String,
}

impl Default for MockPlatform {
    fn default() -> Self {
        MockPlatform {
            inner: Mutex::new(Inner {
                windows: vec![
                    WindowInfo {
                        id: 1,
                        title: "Terminal".into(),
                        pid: 1001,
                        focused: true,
                    },
                    WindowInfo {
                        id: 2,
                        title: "Firefox".into(),
                        pid: 1002,
                        focused: false,
                    },
                    WindowInfo {
                        id: 3,
                        title: "KeePassXC".into(),
                        pid: 1003,
                        focused: false,
                    },
                ],
                processes: vec![
                    ProcInfo {
                        pid: 1001,
                        name: "bash".into(),
                        rss_kb: 4096,
                    },
                    ProcInfo {
                        pid: 1002,
                        name: "firefox".into(),
                        rss_kb: 900_000,
                    },
                    ProcInfo {
                        pid: 1003,
                        name: "keepassxc".into(),
                        rss_kb: 80_000,
                    },
                ],
                typed: String::new(),
            }),
        }
    }
}

impl MockPlatform {
    pub fn new() -> Self {
        Self::default()
    }
    /// Test helper: what has been typed so far.
    pub fn typed_text(&self) -> String {
        self.inner.lock().unwrap().typed.clone()
    }
}

impl Platform for MockPlatform {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError> {
        Ok(self.inner.lock().unwrap().windows.clone())
    }

    fn list_processes(&self) -> Result<Vec<ProcInfo>, PalError> {
        Ok(self.inner.lock().unwrap().processes.clone())
    }

    fn focus_window(&self, id: u64) -> Result<(), PalError> {
        let mut g = self.inner.lock().unwrap();
        if !g.windows.iter().any(|w| w.id == id) {
            return Err(PalError::NoWindow(id));
        }
        for w in &mut g.windows {
            w.focused = w.id == id;
        }
        Ok(())
    }

    fn kill_process(&self, pid: u32) -> Result<(), PalError> {
        let mut g = self.inner.lock().unwrap();
        let before = g.processes.len();
        g.processes.retain(|p| p.pid != pid);
        if g.processes.len() == before {
            return Err(PalError::NoProcess(pid));
        }
        g.windows.retain(|w| w.pid != pid);
        Ok(())
    }

    fn focused_process_name(&self) -> Result<String, PalError> {
        let g = self.inner.lock().unwrap();
        let focused = g.windows.iter().find(|w| w.focused);
        match focused {
            Some(w) => {
                let name = g
                    .processes
                    .iter()
                    .find(|p| p.pid == w.pid)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                Ok(name)
            }
            None => Err(PalError::Backend("no focused window".into())),
        }
    }

    fn type_text(&self, text: &str) -> Result<(), PalError> {
        self.inner.lock().unwrap().typed.push_str(text);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_moves_and_reports_process() {
        let p = MockPlatform::new();
        p.focus_window(2).unwrap();
        assert_eq!(p.focused_process_name().unwrap(), "firefox");
    }

    #[test]
    fn kill_removes_process_and_windows() {
        let p = MockPlatform::new();
        p.kill_process(1002).unwrap();
        assert!(p.list_processes().unwrap().iter().all(|x| x.pid != 1002));
        assert!(p.list_windows().unwrap().iter().all(|w| w.pid != 1002));
        assert!(matches!(p.kill_process(1002), Err(PalError::NoProcess(_))));
    }

    #[test]
    fn typing_accumulates() {
        let p = MockPlatform::new();
        p.type_text("hello ").unwrap();
        p.type_text("world").unwrap();
        assert_eq!(p.typed_text(), "hello world");
    }
}
