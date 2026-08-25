//! UI Automation backend — the accessibility tree (read) and synthetic clicks
//! (invoke), Windows only.
//!
//! IUIAutomation is a COM API. The rest of the Win32 PAL (`super`) talks to raw
//! `windows-sys`, but hand-rolling COM vtables/refcounting there would be
//! miserable, so this module alone pulls in the higher-level `windows` crate for
//! its proper COM wrappers (Drop-based refcounting, `Result` errors). The two
//! worlds never touch: everything here is self-contained and exposes just two
//! free functions back to `super::WindowsPlatform`.
//!
//! Not built in the Linux CI sandbox (the whole `windows` module is
//! `#[cfg(windows)]`); exercised on the Windows target.

use crate::pal::PalError;
use oracle_ipc::actd::UiElement;
use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationInvokePattern,
    IUIAutomationLegacyIAccessiblePattern, IUIAutomationSelectionItemPattern,
    IUIAutomationTogglePattern, IUIAutomationTreeWalker, IUIAutomationValuePattern,
    UIA_InvokePatternId, UIA_LegacyIAccessiblePatternId, UIA_SelectionItemPatternId,
    UIA_TogglePatternId, UIA_ValuePatternId,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// Hard ceiling on tree nodes we *visit* (walk into), regardless of how many we
/// keep. Bounds cost on huge apps and guards against pathological walks.
const VISIT_CAP: u32 = 4000;
/// Hard ceiling on elements we *report* — enough to find any control, small
/// enough to keep the JSON and the model's context sane.
const ELEMENT_CAP: usize = 300;

/// A reported node plus its live COM handle (so `invoke_element` can act on the
/// very element it matched).
struct Node {
    info: UiElement,
    handle: IUIAutomationElement,
}

struct Collector {
    out: Vec<Node>,
    visits: u32,
    max_depth: u32,
}

/// Read the control tree of a window (or the foreground window).
pub fn read_ui_tree(window_id: Option<u64>, max_depth: u32) -> Result<Vec<UiElement>, PalError> {
    unsafe {
        let auto = automation()?;
        let root = root_element(&auto, window_id)?;
        let walker = auto
            .ControlViewWalker()
            .map_err(|e| PalError::Backend(format!("UIA tree walker: {e}")))?;
        let mut c = Collector {
            out: Vec::new(),
            visits: 0,
            max_depth,
        };
        c.walk(&walker, root, 0);
        Ok(c.out.into_iter().map(|n| n.info).collect())
    }
}

/// Find the first element whose name matches (and, if given, whose control type
/// matches) and invoke its default action.
pub fn invoke_element(
    window_id: Option<u64>,
    name: &str,
    control_type: Option<&str>,
) -> Result<UiElement, PalError> {
    unsafe {
        let auto = automation()?;
        let root = root_element(&auto, window_id)?;
        let walker = auto
            .ControlViewWalker()
            .map_err(|e| PalError::Backend(format!("UIA tree walker: {e}")))?;
        // Walk deep enough to reach nested controls; the caps still bound it.
        let mut c = Collector {
            out: Vec::new(),
            visits: 0,
            max_depth: 40,
        };
        c.walk(&walker, root, 0);

        let want = name.trim().to_lowercase();
        let want_ct = control_type.map(|s| s.trim().to_lowercase());
        let node = c.out.into_iter().find(|n| {
            n.info.name.to_lowercase().contains(&want)
                && want_ct
                    .as_ref()
                    .map(|ct| n.info.control_type.contains(ct))
                    .unwrap_or(true)
        });
        let Some(node) = node else {
            return Err(PalError::NoElement(name.to_string()));
        };
        if try_invoke(&node.handle) {
            Ok(node.info)
        } else {
            Err(PalError::Backend(format!(
                "'{}' ({}) exposes no invokable action",
                node.info.name, node.info.control_type
            )))
        }
    }
}

/// Bring up an IUIAutomation instance, initialising COM on this thread first.
fn automation() -> Result<IUIAutomation, PalError> {
    unsafe {
        // COM is per-thread; these PAL calls may land on any worker thread.
        // Benign results (already initialised / mode already chosen) are fine —
        // we only need COM up, in whatever apartment it is in. UIA works in both.
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let auto: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| PalError::Backend(format!("UI Automation unavailable: {e}")))?;
        Ok(auto)
    }
}

/// The root element to walk from: an explicit window handle, or the foreground
/// window when none is given.
unsafe fn root_element(
    auto: &IUIAutomation,
    window_id: Option<u64>,
) -> Result<IUIAutomationElement, PalError> {
    let hwnd = match window_id {
        Some(id) => HWND(id as usize as *mut core::ffi::c_void),
        None => {
            let h = GetForegroundWindow();
            if h.0.is_null() {
                return Err(PalError::Backend("no foreground window to read".into()));
            }
            h
        }
    };
    auto.ElementFromHandle(hwnd)
        .map_err(|e| PalError::Backend(format!("no UI element for that window: {e}")))
}

impl Collector {
    unsafe fn walk(
        &mut self,
        walker: &IUIAutomationTreeWalker,
        el: IUIAutomationElement,
        depth: u32,
    ) {
        if self.out.len() >= ELEMENT_CAP || self.visits >= VISIT_CAP {
            return;
        }
        self.visits += 1;

        // A live element answers CurrentControlType; a null/stale handle errors.
        // That is how we detect "no more children" without relying on the exact
        // null-vs-Err behaviour of the walker's navigation methods.
        let ct = match el.CurrentControlType() {
            Ok(c) => c.0,
            Err(_) => return,
        };
        let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
        let value = el
            .GetCurrentPattern(UIA_ValuePatternId)
            .and_then(|u| u.cast::<IUIAutomationValuePattern>())
            .and_then(|vp| vp.CurrentValue())
            .ok()
            .map(|b| b.to_string())
            .filter(|s| !s.is_empty());
        let enabled = el.CurrentIsEnabled().map(|b| b.as_bool()).unwrap_or(false);
        let rect = el
            .CurrentBoundingRectangle()
            .ok()
            .map(|r| [r.left, r.top, r.right - r.left, r.bottom - r.top]);
        let control_type = control_type_name(ct);

        if is_reportable(&control_type, &name, &value) {
            self.out.push(Node {
                info: UiElement {
                    depth,
                    control_type,
                    name,
                    value,
                    enabled,
                    rect,
                },
                handle: el.clone(),
            });
        }

        if depth >= self.max_depth {
            return;
        }
        let mut child = match walker.GetFirstChildElement(&el) {
            Ok(c) => c,
            Err(_) => return,
        };
        loop {
            if self.out.len() >= ELEMENT_CAP || self.visits >= VISIT_CAP {
                break;
            }
            self.walk(walker, child.clone(), depth + 1);
            child = match walker.GetNextSiblingElement(&child) {
                Ok(s) => s,
                Err(_) => break,
            };
        }
    }
}

/// Try each actuation pattern in turn; the first that succeeds wins.
unsafe fn try_invoke(el: &IUIAutomationElement) -> bool {
    if let Ok(p) = el
        .GetCurrentPattern(UIA_InvokePatternId)
        .and_then(|u| u.cast::<IUIAutomationInvokePattern>())
    {
        if p.Invoke().is_ok() {
            return true;
        }
    }
    if let Ok(p) = el
        .GetCurrentPattern(UIA_TogglePatternId)
        .and_then(|u| u.cast::<IUIAutomationTogglePattern>())
    {
        if p.Toggle().is_ok() {
            return true;
        }
    }
    if let Ok(p) = el
        .GetCurrentPattern(UIA_SelectionItemPatternId)
        .and_then(|u| u.cast::<IUIAutomationSelectionItemPattern>())
    {
        if p.Select().is_ok() {
            return true;
        }
    }
    if let Ok(p) = el
        .GetCurrentPattern(UIA_LegacyIAccessiblePatternId)
        .and_then(|u| u.cast::<IUIAutomationLegacyIAccessiblePattern>())
    {
        if p.DoDefaultAction().is_ok() {
            return true;
        }
    }
    false
}

/// Report an element if it is labelled, carries a value, or is an inherently
/// interactive control (so nameless buttons still surface). Non-reportable
/// nodes are still walked *through* — we just don't list them.
fn is_reportable(control_type: &str, name: &str, value: &Option<String>) -> bool {
    if !name.is_empty() || value.is_some() {
        return true;
    }
    matches!(
        control_type,
        "button"
            | "menuitem"
            | "checkbox"
            | "radiobutton"
            | "combobox"
            | "edit"
            | "hyperlink"
            | "listitem"
            | "tab"
            | "tabitem"
            | "treeitem"
            | "splitbutton"
            | "slider"
            | "spinner"
    )
}

/// Map a UIA control-type id to a short lowercase name (winuser UIA_*ControlTypeId).
fn control_type_name(ct: i32) -> String {
    let s = match ct {
        50000 => "button",
        50001 => "calendar",
        50002 => "checkbox",
        50003 => "combobox",
        50004 => "edit",
        50005 => "hyperlink",
        50006 => "image",
        50007 => "listitem",
        50008 => "list",
        50009 => "menu",
        50010 => "menubar",
        50011 => "menuitem",
        50012 => "progressbar",
        50013 => "radiobutton",
        50014 => "scrollbar",
        50015 => "slider",
        50016 => "spinner",
        50017 => "statusbar",
        50018 => "tab",
        50019 => "tabitem",
        50020 => "text",
        50021 => "toolbar",
        50022 => "tooltip",
        50023 => "tree",
        50024 => "treeitem",
        50025 => "custom",
        50026 => "group",
        50027 => "thumb",
        50028 => "datagrid",
        50029 => "dataitem",
        50030 => "document",
        50031 => "splitbutton",
        50032 => "window",
        50033 => "pane",
        50034 => "header",
        50035 => "headeritem",
        50036 => "table",
        50037 => "titlebar",
        50038 => "separator",
        50039 => "semanticzoom",
        50040 => "appbar",
        other => return format!("type{other}"),
    };
    s.to_string()
}
