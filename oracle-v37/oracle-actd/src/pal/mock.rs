//! In-memory Platform backend: models windows/processes/focus/typing so the
//! daemon's policy + RPC + audit paths are fully testable on any OS.

use super::{PalError, Platform};
use oracle_ipc::actd::{MediaKey, ProcInfo, UiElement, WindowInfo};
use std::sync::Mutex;

pub struct MockPlatform {
    inner: Mutex<Inner>,
}

struct Inner {
    windows: Vec<WindowInfo>,
    processes: Vec<ProcInfo>,
    typed: String,
    opened: Vec<String>,
    media: Vec<MediaKey>,
    invoked: Vec<String>,
}

/// A fixed synthetic control tree so the daemon's observe/act paths are
/// testable without a real desktop. Models a tiny "editor" window.
fn mock_ui_tree() -> Vec<UiElement> {
    vec![
        UiElement {
            depth: 0,
            control_type: "window".into(),
            name: "Editor".into(),
            value: None,
            enabled: true,
            rect: Some([0, 0, 800, 600]),
        },
        UiElement {
            depth: 1,
            control_type: "edit".into(),
            name: "Document".into(),
            value: Some("hello world".into()),
            enabled: true,
            rect: Some([8, 40, 784, 500]),
        },
        UiElement {
            depth: 1,
            control_type: "button".into(),
            name: "Save".into(),
            value: None,
            enabled: true,
            rect: Some([8, 8, 60, 24]),
        },
        UiElement {
            depth: 1,
            control_type: "button".into(),
            name: "Cancel".into(),
            value: None,
            enabled: true,
            rect: Some([72, 8, 60, 24]),
        },
    ]
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
                        minimized: false,
                    },
                    WindowInfo {
                        id: 2,
                        title: "Firefox".into(),
                        pid: 1002,
                        focused: false,
                        minimized: false,
                    },
                    WindowInfo {
                        id: 3,
                        title: "KeePassXC".into(),
                        pid: 1003,
                        focused: false,
                        minimized: false,
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
                opened: Vec::new(),
                media: Vec::new(),
                invoked: Vec::new(),
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
    /// Test helper: targets opened so far.
    pub fn opened_targets(&self) -> Vec<String> {
        self.inner.lock().unwrap().opened.clone()
    }
    /// Test helper: media keys tapped so far.
    pub fn media_keys(&self) -> Vec<MediaKey> {
        self.inner.lock().unwrap().media.clone()
    }
    /// Test helper: element names invoked so far.
    pub fn invoked_elements(&self) -> Vec<String> {
        self.inner.lock().unwrap().invoked.clone()
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

    fn open_target(&self, target: &str) -> Result<(), PalError> {
        if target.trim().is_empty() {
            return Err(PalError::Backend("empty target".into()));
        }
        self.inner.lock().unwrap().opened.push(target.to_string());
        Ok(())
    }

    fn media_key(&self, key: MediaKey) -> Result<(), PalError> {
        self.inner.lock().unwrap().media.push(key);
        Ok(())
    }

    fn window_op(&self, id: u64, _op: oracle_ipc::actd::WindowOp) -> Result<(), PalError> {
        let g = self.inner.lock().unwrap();
        if g.windows.iter().any(|w| w.id == id) {
            Ok(())
        } else {
            Err(PalError::NoWindow(id))
        }
    }

    fn lock_screen(&self) -> Result<(), PalError> {
        Ok(())
    }

    fn read_ui_tree(
        &self,
        window_id: Option<u64>,
        max_depth: u32,
    ) -> Result<Vec<UiElement>, PalError> {
        // If a specific window is named, it must exist; the tree itself is the
        // same fixed model regardless.
        if let Some(id) = window_id {
            let g = self.inner.lock().unwrap();
            if !g.windows.iter().any(|w| w.id == id) {
                return Err(PalError::NoWindow(id));
            }
        }
        Ok(mock_ui_tree()
            .into_iter()
            .filter(|e| e.depth <= max_depth)
            .collect())
    }

    fn invoke_element(
        &self,
        window_id: Option<u64>,
        name: &str,
        control_type: Option<&str>,
    ) -> Result<UiElement, PalError> {
        if let Some(id) = window_id {
            let g = self.inner.lock().unwrap();
            if !g.windows.iter().any(|w| w.id == id) {
                return Err(PalError::NoWindow(id));
            }
        }
        let want = name.trim().to_lowercase();
        let hit = mock_ui_tree().into_iter().find(|e| {
            e.name.to_lowercase().contains(&want)
                && control_type
                    .map(|ct| e.control_type.contains(&ct.trim().to_lowercase()))
                    .unwrap_or(true)
        });
        match hit {
            Some(el) => {
                self.inner.lock().unwrap().invoked.push(el.name.clone());
                Ok(el)
            }
            None => Err(PalError::NoElement(name.to_string())),
        }
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

    #[test]
    fn reads_ui_tree_and_respects_depth() {
        let p = MockPlatform::new();
        let all = p.read_ui_tree(None, 10).unwrap();
        assert!(all.iter().any(|e| e.name == "Save" && e.control_type == "button"));
        // depth 0 keeps only the window root.
        let shallow = p.read_ui_tree(None, 0).unwrap();
        assert_eq!(shallow.len(), 1);
        assert_eq!(shallow[0].control_type, "window");
    }

    #[test]
    fn invokes_element_by_name_and_records_it() {
        let p = MockPlatform::new();
        let el = p.invoke_element(None, "save", None).unwrap();
        assert_eq!(el.name, "Save");
        assert_eq!(p.invoked_elements(), vec!["Save".to_string()]);
        // control-type filter that can't match → miss.
        assert!(matches!(
            p.invoke_element(None, "save", Some("edit")),
            Err(PalError::NoElement(_))
        ));
        // unknown name → miss.
        assert!(matches!(
            p.invoke_element(None, "frobnicate", None),
            Err(PalError::NoElement(_))
        ));
    }

    #[test]
    fn ui_ops_reject_unknown_window() {
        let p = MockPlatform::new();
        assert!(matches!(
            p.read_ui_tree(Some(99999), 10),
            Err(PalError::NoWindow(_))
        ));
        assert!(matches!(
            p.invoke_element(Some(99999), "save", None),
            Err(PalError::NoWindow(_))
        ));
    }
}
