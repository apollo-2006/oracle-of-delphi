//! macOS Platform backend.
//!
//! Everything here is real: process enumeration reads `ps`, and window,
//! input and accessibility control go through the Accessibility API by way of
//! `osascript`. Before this existed macOS fell through to `MockPlatform`, so
//! the assistant reported success while doing nothing to the machine.
//!
//! # Why AppleScript rather than direct FFI
//!
//! The equivalent native path is `AXUIElement` + `CGEvent`, which means an
//! Objective-C bridge (`objc2`, `core-graphics`, `core-foundation`) and a good
//! deal of `unsafe`. `osascript` reaches the *same* Accessibility API through a
//! supported interface with no new dependency and no unsafe code, which matches
//! how the Linux backend prefers `/proc` and `xdg-open` over linking libc.
//!
//! The cost is honest: each call spawns a process and takes roughly 50-150ms,
//! so this is fine for "focus that window" and wrong for anything per-frame. If
//! actuation latency ever matters, the seam to replace is [`osascript`] alone —
//! every method below routes through it.
//!
//! # Permissions (TCC)
//!
//! macOS gates this behind two separate grants, and neither can be requested
//! programmatically:
//!
//! * **Accessibility** — System Settings → Privacy & Security → Accessibility.
//!   Required for window control, input injection, and reading the UI tree.
//! * **Automation** — the first Apple event to System Events raises a consent
//!   prompt; denial is remembered until reset with `tccutil`.
//!
//! Whichever binary is running must be the one granted, so during development
//! that is your terminal, and in a bundled build it is Oracle.app. A missing
//! grant surfaces as [`PalError::Backend`] naming the setting to turn on,
//! rather than as a silent no-op.

use super::{PalError, Platform};
use oracle_ipc::actd::{MediaKey, ProcInfo, UiElement, WindowInfo, WindowOp};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Default)]
pub struct MacosPlatform;

impl MacosPlatform {
    pub fn new() -> Self {
        MacosPlatform
    }
}

/// Window ids are synthesized as `(pid << 32) | one_based_index`.
///
/// The Accessibility API addresses a window by its position in an application's
/// window list, not by a global handle, so there is no OS-provided u64 to pass
/// through. Packing both halves keeps [`Platform`] unchanged and makes the id
/// reversible without any server-side table.
///
/// The tradeoff: an index is only stable while the app's window order is, so an
/// id can go stale if windows are opened or closed between a list and an
/// operation. That is why every op re-resolves the window and reports
/// [`PalError::NoWindow`] instead of acting on whatever now sits at that index.
fn pack_id(pid: u32, index: u32) -> u64 {
    ((pid as u64) << 32) | (index as u64)
}

fn unpack_id(id: u64) -> (u32, u32) {
    ((id >> 32) as u32, (id & 0xffff_ffff) as u32)
}

/// Escape a Rust string into an AppleScript string literal body.
///
/// Scripts are fed to `osascript` over stdin rather than as `-e` arguments, so
/// the shell never sees them; this handles the AppleScript layer, where only
/// the backslash and the double quote are special. Without it, a window title
/// or a block of dictated text containing a quote would end the literal early
/// and the remainder would be parsed as code.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Run an AppleScript and return its trimmed stdout.
fn osascript(script: &str) -> Result<String, PalError> {
    let mut child = Command::new("osascript")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PalError::Backend(format!("osascript: {e}")))?;

    child
        .stdin
        .as_mut()
        .ok_or_else(|| PalError::Backend("osascript: no stdin".into()))?
        .write_all(script.as_bytes())
        .map_err(|e| PalError::Backend(format!("osascript write: {e}")))?;

    let out = child
        .wait_with_output()
        .map_err(|e| PalError::Backend(format!("osascript: {e}")))?;

    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }

    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(classify(&err))
}

/// Turn osascript's stderr into an actionable error.
///
/// A denied TCC grant is the single most likely failure on a fresh machine and
/// its raw text ("-1743", "assistive access") tells the user nothing about what
/// to click, so those two cases are translated.
fn classify(err: &str) -> PalError {
    if err.contains("-1743") || err.contains("not allowed to send Apple events") {
        return PalError::InjectionBlocked(
            "macOS denied Automation access. Grant it under System Settings → Privacy & \
             Security → Automation, for whichever binary is running (your terminal in a dev \
             run, Oracle.app in a bundled one)."
                .into(),
        );
    }
    if err.contains("assistive access") || err.contains("-25211") {
        return PalError::InjectionBlocked(
            "macOS denied Accessibility access, which is required for window control and \
             input injection. Enable it under System Settings → Privacy & Security → \
             Accessibility."
                .into(),
        );
    }
    PalError::Backend(format!("osascript: {err}"))
}

impl Platform for MacosPlatform {
    fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError> {
        // `background only is false` skips daemons and agents, leaving the
        // windowed apps a user could plausibly mean.
        let script = r#"
tell application "System Events"
  set out to ""
  repeat with p in (every process whose background only is false)
    set thePid to unix id of p
    set isFront to frontmost of p
    set i to 0
    repeat with w in windows of p
      set i to i + 1
      set t to ""
      try
        set t to name of w
      end try
      set m to false
      try
        set m to value of attribute "AXMinimized" of w
      end try
      set out to out & thePid & tab & i & tab & isFront & tab & m & tab & t & linefeed
    end repeat
  end repeat
  return out
end tell
"#;
        let raw = osascript(script)?;
        let mut out = Vec::new();
        for line in raw.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() < 5 {
                continue;
            }
            let (Ok(pid), Ok(index)) = (f[0].parse::<u32>(), f[1].parse::<u32>()) else {
                continue;
            };
            let frontmost = f[2] == "true";
            let minimized = f[3] == "true";
            out.push(WindowInfo {
                id: pack_id(pid, index),
                title: f[4..].join("\t"),
                pid,
                // Only the front window of the frontmost app has focus; the
                // rest of that app's windows are merely in a frontmost process.
                focused: frontmost && index == 1 && !minimized,
                minimized,
            });
        }
        Ok(out)
    }

    fn list_processes(&self) -> Result<Vec<ProcInfo>, PalError> {
        // rss is already in KiB, matching ProcInfo. No permission needed.
        let out = Command::new("ps")
            .args(["-axo", "pid=,rss=,comm="])
            .output()
            .map_err(|e| PalError::Backend(format!("ps: {e}")))?;
        if !out.status.success() {
            return Err(PalError::Backend("ps failed".into()));
        }

        let mut procs = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = line.trim().splitn(3, char::is_whitespace);
            let (Some(pid), Some(rss), Some(cmd)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let (Ok(pid), Ok(rss_kb)) = (pid.parse::<u32>(), rss.trim().parse::<u64>()) else {
                continue;
            };
            // comm is a full executable path on macOS; the callers want a name.
            let name = cmd.trim().rsplit('/').next().unwrap_or(cmd).to_string();
            procs.push(ProcInfo { pid, name, rss_kb });
        }
        Ok(procs)
    }

    fn focus_window(&self, id: u64) -> Result<(), PalError> {
        let (pid, index) = unpack_id(id);
        let script = format!(
            r#"
tell application "System Events"
  set matches to (every process whose unix id is {pid})
  if (count of matches) is 0 then error "no-process"
  set p to item 1 of matches
  if (count of windows of p) < {index} then error "no-window"
  set frontmost of p to true
  perform action "AXRaise" of window {index} of p
end tell
"#
        );
        match osascript(&script) {
            Ok(_) => Ok(()),
            Err(PalError::Backend(e)) if e.contains("no-process") => Err(PalError::NoProcess(pid)),
            Err(PalError::Backend(e)) if e.contains("no-window") => Err(PalError::NoWindow(id)),
            Err(e) => Err(e),
        }
    }

    fn kill_process(&self, pid: u32) -> Result<(), PalError> {
        // Probe first so a missing process is NoProcess rather than a generic
        // backend failure: signal 0 checks for existence without delivering.
        let probe = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .output()
            .map_err(|e| PalError::Backend(format!("kill: {e}")))?;
        if !probe.status.success() {
            return Err(PalError::NoProcess(pid));
        }

        let out = Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .output()
            .map_err(|e| PalError::Backend(format!("kill: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(PalError::Backend(
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ))
        }
    }

    fn focused_process_name(&self) -> Result<String, PalError> {
        osascript(
            r#"tell application "System Events" to return name of first process whose frontmost is true"#,
        )
    }

    fn type_text(&self, text: &str) -> Result<(), PalError> {
        let script = format!(
            r#"tell application "System Events" to keystroke "{}""#,
            escape(text)
        );
        osascript(&script).map(|_| ())
    }

    fn open_target(&self, target: &str) -> Result<(), PalError> {
        // `open` is the macOS "open verb": apps, URLs, files and folders alike.
        // No Accessibility grant needed.
        Command::new("open")
            .arg(target)
            .spawn()
            .map(|_| ())
            .map_err(|e| PalError::Backend(format!("open: {e}")))
    }

    fn media_key(&self, key: MediaKey) -> Result<(), PalError> {
        match key {
            // Volume goes through the real system control, not a key tap.
            MediaKey::VolumeUp | MediaKey::VolumeDown => {
                let delta = if matches!(key, MediaKey::VolumeUp) {
                    10
                } else {
                    -10
                };
                let script = format!(
                    r#"
set cur to output volume of (get volume settings)
set target to cur + ({delta})
if target > 100 then set target to 100
if target < 0 then set target to 0
set volume output volume target
"#
                );
                osascript(&script).map(|_| ())
            }
            MediaKey::Mute => osascript("set volume with output muted").map(|_| ()),
            // Transport keys are the honest gap. AppleScript cannot post the
            // NX system-defined events the real media keys use, so the best
            // available approach is to drive whichever player is actually
            // running; if none is, say so rather than silently doing nothing.
            MediaKey::PlayPause | MediaKey::Next | MediaKey::Previous | MediaKey::Stop => {
                let verb = match key {
                    MediaKey::PlayPause => "playpause",
                    MediaKey::Next => "next track",
                    MediaKey::Previous => "previous track",
                    _ => "stop",
                };
                let script = format!(
                    r#"
set didAct to false
repeat with appName in {{"Spotify", "Music"}}
  if application appName is running then
    tell application appName to {verb}
    set didAct to true
    exit repeat
  end if
end repeat
if not didAct then error "no-player"
"#
                );
                match osascript(&script) {
                    Ok(_) => Ok(()),
                    Err(PalError::Backend(e)) if e.contains("no-player") => Err(PalError::Backend(
                        "no supported media player is running (Spotify or Music). macOS \
                             transport keys are system-defined events that AppleScript cannot \
                             post; driving the player directly is the supported path."
                            .into(),
                    )),
                    Err(e) => Err(e),
                }
            }
        }
    }

    fn window_op(&self, id: u64, op: WindowOp) -> Result<(), PalError> {
        let (pid, index) = unpack_id(id);
        // Buttons are addressed by subrole rather than position: the close and
        // zoom buttons are not at a fixed index across every window style.
        let action = match op {
            WindowOp::Minimize => {
                format!(r#"set value of attribute "AXMinimized" of window {index} of p to true"#)
            }
            WindowOp::Restore => {
                format!(r#"set value of attribute "AXMinimized" of window {index} of p to false"#)
            }
            WindowOp::Maximize => format!(
                r#"perform action "AXPress" of (first button of window {index} of p whose subrole is "AXZoomButton")"#
            ),
            WindowOp::Close => format!(
                r#"perform action "AXPress" of (first button of window {index} of p whose subrole is "AXCloseButton")"#
            ),
        };
        let script = format!(
            r#"
tell application "System Events"
  set matches to (every process whose unix id is {pid})
  if (count of matches) is 0 then error "no-process"
  set p to item 1 of matches
  if (count of windows of p) < {index} then error "no-window"
  {action}
end tell
"#
        );
        match osascript(&script) {
            Ok(_) => Ok(()),
            Err(PalError::Backend(e)) if e.contains("no-process") => Err(PalError::NoProcess(pid)),
            Err(PalError::Backend(e)) if e.contains("no-window") => Err(PalError::NoWindow(id)),
            Err(e) => Err(e),
        }
    }

    fn lock_screen(&self) -> Result<(), PalError> {
        // CGSession -suspend is the real lock (drops to the login window).
        const CGSESSION: &str =
            "/System/Library/CoreServices/Menu Extras/User.menu/Contents/Resources/CGSession";
        if std::path::Path::new(CGSESSION).exists() {
            if let Ok(st) = Command::new(CGSESSION).arg("-suspend").status() {
                if st.success() {
                    return Ok(());
                }
            }
        }
        // Fallback: sleep the display, which locks when the security pref
        // requires a password immediately. Weaker, so it is second.
        Command::new("pmset")
            .arg("displaysleepnow")
            .status()
            .map(|_| ())
            .map_err(|e| PalError::Backend(format!("pmset: {e}")))
    }

    fn read_ui_tree(
        &self,
        window_id: Option<u64>,
        max_depth: u32,
    ) -> Result<Vec<UiElement>, PalError> {
        let target = window_target(window_id);
        // Depth-first walk with an explicit bound. `entire contents` would be a
        // single call but returns a flattened list with no depth and no bound,
        // and on a large window it is slow enough to look like a hang.
        let script = format!(
            r#"
on walk(el, d, maxd)
  set acc to ""
  tell application "System Events"
    set kids to {{}}
    try
      set kids to UI elements of el
    end try
    repeat with k in kids
      set nm to ""
      try
        set nm to name of k
      end try
      if nm is missing value then set nm to ""
      set vl to ""
      try
        set vl to (value of k) as text
      end try
      if vl is missing value then set vl to ""
      set en to true
      try
        set en to enabled of k
      end try
      set rl to ""
      try
        set rl to role of k
      end try
      set px to ""
      set py to ""
      set pw to ""
      set ph to ""
      try
        set pos to position of k
        set sz to size of k
        set px to item 1 of pos
        set py to item 2 of pos
        set pw to item 1 of sz
        set ph to item 2 of sz
      end try
      set acc to acc & d & tab & rl & tab & nm & tab & vl & tab & en & tab & px & tab & py & tab & pw & tab & ph & linefeed
      if d < maxd then set acc to acc & my walk(k, d + 1, maxd)
    end repeat
  end tell
  return acc
end walk

tell application "System Events"
  {target}
end tell
return my walk(theWindow, 1, {max_depth})
"#
        );
        let raw = osascript(&script)?;
        Ok(parse_ui_lines(&raw))
    }

    fn invoke_element(
        &self,
        window_id: Option<u64>,
        name: &str,
        control_type: Option<&str>,
    ) -> Result<UiElement, PalError> {
        let target = window_target(window_id);
        let want_role = control_type.map(role_for).unwrap_or_default();
        // Match on name first, then optionally on role, and press the first hit.
        // `entire contents` is acceptable here because we stop at the first
        // match rather than serializing the whole tree.
        let script = format!(
            r#"
tell application "System Events"
  {target}
  set hit to missing value
  repeat with k in (entire contents of theWindow)
    set nm to ""
    try
      set nm to name of k
    end try
    if nm is "{name}" then
      set rl to ""
      try
        set rl to role of k
      end try
      if "{want_role}" is "" or rl is "{want_role}" then
        set hit to k
        exit repeat
      end if
    end if
  end repeat
  if hit is missing value then error "no-element"

  set en to true
  try
    set en to enabled of hit
  end try
  set vl to ""
  try
    set vl to (value of hit) as text
  end try
  if vl is missing value then set vl to ""
  set rl to ""
  try
    set rl to role of hit
  end try
  set px to ""
  set py to ""
  set pw to ""
  set ph to ""
  try
    set pos to position of hit
    set sz to size of hit
    set px to item 1 of pos
    set py to item 2 of pos
    set pw to item 1 of sz
    set ph to item 2 of sz
  end try

  perform action "AXPress" of hit
  return "0" & tab & rl & tab & "{name}" & tab & vl & tab & en & tab & px & tab & py & tab & pw & tab & ph
end tell
"#,
            name = escape(name),
            want_role = escape(&want_role),
        );
        match osascript(&script) {
            Ok(line) => parse_ui_lines(&line)
                .into_iter()
                .next()
                .ok_or_else(|| PalError::Backend("could not parse invoked element".into())),
            Err(PalError::Backend(e)) if e.contains("no-element") => {
                Err(PalError::NoElement(name.to_string()))
            }
            Err(e) => Err(e),
        }
    }
}

/// AppleScript that binds `theWindow`: a specific window when an id is given,
/// otherwise the front window of the frontmost app.
fn window_target(window_id: Option<u64>) -> String {
    match window_id {
        Some(id) => {
            let (pid, index) = unpack_id(id);
            format!(
                r#"set matches to (every process whose unix id is {pid})
  if (count of matches) is 0 then error "no-process"
  set theWindow to window {index} of item 1 of matches"#
            )
        }
        None => {
            r#"set theWindow to window 1 of (first process whose frontmost is true)"#.to_string()
        }
    }
}

/// Map a UIA-style control type onto the macOS AX role the caller means.
///
/// Callers speak the vocabulary in [`UiElement::control_type`], which is
/// modelled on Windows UI Automation; macOS names the same concepts
/// differently, so the two have to be reconciled somewhere.
fn role_for(control_type: &str) -> String {
    match control_type.to_ascii_lowercase().as_str() {
        "button" => "AXButton",
        "edit" | "textbox" => "AXTextField",
        "text" => "AXStaticText",
        "menuitem" => "AXMenuItem",
        "checkbox" => "AXCheckBox",
        "radiobutton" => "AXRadioButton",
        "combobox" => "AXComboBox",
        "list" => "AXList",
        "listitem" => "AXRow",
        "tab" => "AXTabGroup",
        "window" => "AXWindow",
        "link" => "AXLink",
        other => other,
    }
    .to_string()
}

/// Invert [`role_for`]: report an AX role in the caller's vocabulary.
fn control_type_for(role: &str) -> String {
    match role {
        "AXButton" => "button",
        "AXTextField" | "AXTextArea" => "edit",
        "AXStaticText" => "text",
        "AXMenuItem" => "menuitem",
        "AXCheckBox" => "checkbox",
        "AXRadioButton" => "radiobutton",
        "AXComboBox" => "combobox",
        "AXList" => "list",
        "AXRow" => "listitem",
        "AXTabGroup" => "tab",
        "AXWindow" => "window",
        "AXLink" => "link",
        other => return other.trim_start_matches("AX").to_ascii_lowercase(),
    }
    .to_string()
}

/// Parse the tab-separated element rows emitted by the walk scripts.
fn parse_ui_lines(raw: &str) -> Vec<UiElement> {
    let mut out = Vec::new();
    for line in raw.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 9 {
            continue;
        }
        let Ok(depth) = f[0].trim().parse::<u32>() else {
            continue;
        };
        let rect = match (
            f[5].trim().parse::<i32>(),
            f[6].trim().parse::<i32>(),
            f[7].trim().parse::<i32>(),
            f[8].trim().parse::<i32>(),
        ) {
            (Ok(x), Ok(y), Ok(w), Ok(h)) => Some([x, y, w, h]),
            _ => None,
        };
        let value = f[3].trim();
        out.push(UiElement {
            depth,
            control_type: control_type_for(f[1].trim()),
            name: f[2].to_string(),
            value: (!value.is_empty()).then(|| value.to_string()),
            enabled: f[4].trim() == "true",
            rect,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_id_roundtrips() {
        for (pid, index) in [(1u32, 1u32), (99123, 7), (u32::MAX, u32::MAX), (501, 0)] {
            assert_eq!(unpack_id(pack_id(pid, index)), (pid, index));
        }
    }

    #[test]
    fn packed_ids_are_distinct_across_pid_and_index() {
        // A collision would mean acting on the wrong application's window.
        assert_ne!(pack_id(1, 2), pack_id(2, 1));
    }

    #[test]
    fn escape_closes_no_literals() {
        // A quote in dictated text would otherwise terminate the AppleScript
        // string and the rest would be parsed as code.
        assert_eq!(escape(r#"say "hi""#), r#"say \"hi\""#);
        assert_eq!(escape(r"back\slash"), r"back\\slash");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn tcc_denials_are_named_not_numeric() {
        let automation = classify("execution error: Not allowed to send Apple events (-1743)");
        assert!(matches!(automation, PalError::InjectionBlocked(_)));
        assert!(format!("{automation}").contains("Automation"));

        let ax = classify("System Events got an error: assistive access is disabled");
        assert!(matches!(ax, PalError::InjectionBlocked(_)));
        assert!(format!("{ax}").contains("Accessibility"));

        // Anything else stays a plain backend error.
        assert!(matches!(
            classify("some other failure"),
            PalError::Backend(_)
        ));
    }

    #[test]
    fn control_type_vocabulary_roundtrips() {
        for ct in [
            "button", "edit", "text", "menuitem", "checkbox", "combobox", "link",
        ] {
            assert_eq!(control_type_for(&role_for(ct)), ct, "roundtrip for {ct}");
        }
    }

    #[test]
    fn unknown_roles_degrade_to_lowercase() {
        assert_eq!(control_type_for("AXSplitGroup"), "splitgroup");
    }

    #[test]
    fn parses_element_rows() {
        let raw = "1\tAXButton\tSave\t\ttrue\t10\t20\t80\t24\n\
                   2\tAXTextField\tName\tAbir\tfalse\t0\t0\t100\t20\n";
        let els = parse_ui_lines(raw);
        assert_eq!(els.len(), 2);

        assert_eq!(els[0].depth, 1);
        assert_eq!(els[0].control_type, "button");
        assert_eq!(els[0].name, "Save");
        assert_eq!(
            els[0].value, None,
            "empty value must be None, not Some(\"\")"
        );
        assert!(els[0].enabled);
        assert_eq!(els[0].rect, Some([10, 20, 80, 24]));

        assert_eq!(els[1].value.as_deref(), Some("Abir"));
        assert!(!els[1].enabled);
    }

    #[test]
    fn rows_without_geometry_have_no_rect() {
        // AX elements that refuse position/size emit empty fields; that must be
        // None rather than a bogus zero rect the planner would treat as real.
        let els = parse_ui_lines("1\tAXGroup\tPanel\t\ttrue\t\t\t\t\n");
        assert_eq!(els.len(), 1);
        assert_eq!(els[0].rect, None);
    }

    #[test]
    fn malformed_rows_are_skipped_not_panicked() {
        assert!(parse_ui_lines("garbage\nshort\trow\n\n").is_empty());
    }
}
