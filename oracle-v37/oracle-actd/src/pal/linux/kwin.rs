//! Window enumeration and capture on KDE Plasma, through KWin over D-Bus.
//!
//! Wayland gives a client no way to see other clients' windows; that is the
//! point of it. What exists instead is per-compositor, and this is KWin's:
//!
//! * **Listing** goes through a KWin script. Scripts run inside the compositor
//!   and can read `workspace.stackingOrder`, but cannot return a value over
//!   D-Bus, so the script calls back into an object this connection serves.
//!   Each call carries a nonce, so a stale or forged report is ignored.
//! * **Capture** is `org.kde.KWin.ScreenShot2.CaptureWindow`, which grabs a
//!   window by handle whether or not it is on top. That keeps the Windows
//!   backend's semantics: the sampler can photograph the window *behind* the
//!   HUD instead of the HUD.
//!
//! ScreenShot2 is a restricted interface. KWin allows it only for an executable
//! named by an installed `.desktop` file carrying
//! `X-KDE-DBUS-Restricted-Interfaces=org.kde.KWin.ScreenShot2`, matched against
//! `/proc/<pid>/exe`. That is how Spectacle is allowed, and it is what
//! `scripts/setup.sh kwin` installs for `oracle-actd`. Without it, capture is
//! refused with an error that says so, rather than returning something blank.
//!
//! Every other compositor still gets `Unsupported`: KWin working is not GNOME
//! or wlroots working.

use oracle_ipc::actd::{CapturedImage, WindowInfo};
use std::collections::HashMap;
use std::io::Read;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;
use zbus::zvariant::{OwnedValue, Value};

use super::super::PalError;

const KWIN: &str = "org.kde.KWin";
const BRIDGE_PATH: &str = "/dev/oracle/kwin";
const BRIDGE_IFACE: &str = "dev.oracle.KwinBridge";
/// How long to wait for the script's report. It arrives in a few milliseconds;
/// anything near this means the script failed to run.
const REPORT_TIMEOUT: Duration = Duration::from_secs(2);
/// How long to wait for KWin to finish writing pixels into the pipe.
const PIXELS_TIMEOUT: Duration = Duration::from_secs(5);

/// The object KWin's script calls back into.
struct Bridge {
    tx: Mutex<mpsc::Sender<(String, String)>>,
}

#[zbus::interface(name = "dev.oracle.KwinBridge")]
impl Bridge {
    // Explicit name: zbus PascalCases method names by default, and the script
    // has to spell it exactly.
    #[zbus(name = "Report")]
    fn report(&self, nonce: String, json: String) {
        if let Ok(tx) = self.tx.lock() {
            let _ = tx.send((nonce, json));
        }
    }
}

struct Session {
    conn: zbus::blocking::Connection,
    reports: mpsc::Receiver<(String, String)>,
}

/// One window as the script reports it.
#[derive(Debug, Clone, serde::Deserialize, PartialEq)]
struct KwinWindow {
    /// KWin's internal UUID, e.g. `{35bbf44e-...}`. The capture handle.
    id: String,
    title: String,
    pid: u32,
    active: bool,
    /// Minimized, or on another virtual desktop. Either way, not on screen.
    hidden: bool,
}

#[derive(Default)]
pub struct Kwin {
    session: Mutex<Option<Session>>,
    /// Wire id -> (KWin handle, title) from the most recent listing. The wire
    /// type carries a u64; KWin speaks UUIDs.
    handles: Mutex<HashMap<u64, (String, String)>>,
}

impl Kwin {
    pub fn list_windows(&self) -> Result<Vec<WindowInfo>, PalError> {
        let windows = self.with_session(list_via_script)?;
        let mut handles = self.handles.lock().unwrap_or_else(|e| e.into_inner());
        handles.clear();
        Ok(windows
            .into_iter()
            .map(|w| {
                let id = wire_id(&w.id);
                handles.insert(id, (w.id.clone(), w.title.clone()));
                WindowInfo {
                    id,
                    title: w.title,
                    pid: w.pid,
                    focused: w.active,
                    minimized: w.hidden,
                }
            })
            .collect())
    }

    pub fn capture_window(
        &self,
        window_id: Option<u64>,
        max_width: u32,
    ) -> Result<CapturedImage, PalError> {
        // Resolve the handle and title. A listing refreshes the table, so an id
        // from a listing made by a previous actd call still resolves.
        let (wire, handle, title) = match window_id {
            Some(id) => {
                let known = self.lookup(id);
                let (handle, title) = match known {
                    Some(h) => h,
                    None => {
                        self.list_windows()?;
                        self.lookup(id).ok_or(PalError::NoWindow(id))?
                    }
                };
                (id, Some(handle), title)
            }
            None => {
                let active = self.list_windows()?.into_iter().find(|w| w.focused);
                match active {
                    Some(w) => (w.id, None, w.title),
                    None => (0, None, String::new()),
                }
            }
        };

        let (meta, pixels) = self.with_session(|s| capture(s, handle.as_deref()))?;
        let (rgba, w, h) = to_rgba(&meta, &pixels)?;
        super::super::capture::finish(wire, title, &rgba, w, h, max_width)
    }

    fn lookup(&self, id: u64) -> Option<(String, String)> {
        self.handles
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// Run `f` against the session, connecting on first use.
    ///
    /// The lock also serializes callers, which the script round trip needs: two
    /// listings in flight would each have to sort out the other's report.
    fn with_session<T>(
        &self,
        f: impl FnOnce(&Session) -> Result<T, PalError>,
    ) -> Result<T, PalError> {
        let mut guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(connect()?);
        }
        let result = f(guard.as_ref().expect("connected above"));
        // A dead bus connection never recovers by itself; drop it so the next
        // call reconnects (KWin restarting does not need this, logging out does).
        if let Err(PalError::Backend(msg)) = &result {
            if msg.contains("I/O error") {
                *guard = None;
            }
        }
        result
    }
}

fn connect() -> Result<Session, PalError> {
    let (tx, reports) = mpsc::channel();
    let conn = zbus::blocking::connection::Builder::session()
        .and_then(|b| b.serve_at(BRIDGE_PATH, Bridge { tx: Mutex::new(tx) }))
        .and_then(|b| b.build())
        .map_err(|e| PalError::Backend(format!("session D-Bus: {e}")))?;

    let dbus = zbus::blocking::fdo::DBusProxy::new(&conn).map_err(bus_err)?;
    let has_kwin = dbus
        .name_has_owner(KWIN.try_into().expect("valid bus name"))
        .map_err(bus_err)?;
    if !has_kwin {
        return Err(PalError::Unsupported(
            "window list and capture on Linux need KDE Plasma (no KWin on the session bus)",
        ));
    }
    Ok(Session { conn, reports })
}

fn bus_err(e: impl std::fmt::Display) -> PalError {
    PalError::Backend(format!("KWin D-Bus: {e}"))
}

fn list_via_script(s: &Session) -> Result<Vec<KwinWindow>, PalError> {
    let me = s
        .conn
        .unique_name()
        .ok_or_else(|| PalError::Backend("D-Bus connection has no name".into()))?
        .to_string();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let plugin = format!("oracle-actd-{nonce}");

    let dir = script_dir()?;
    let path = dir.join(format!("{plugin}.js"));
    std::fs::write(&path, list_script(&me, &nonce))
        .map_err(|e| PalError::Backend(format!("writing KWin script: {e}")))?;
    let result = run_script(s, &path, &plugin, &nonce);
    let _ = std::fs::remove_file(&path);
    let json = result?;
    serde_json::from_str(&json)
        .map_err(|e| PalError::Backend(format!("KWin script report did not parse: {e}")))
}

fn run_script(
    s: &Session,
    path: &std::path::Path,
    plugin: &str,
    nonce: &str,
) -> Result<String, PalError> {
    let scripting =
        zbus::blocking::Proxy::new(&s.conn, KWIN, "/Scripting", "org.kde.kwin.Scripting")
            .map_err(bus_err)?;
    let path_str = path.to_string_lossy();
    let id: i32 = scripting
        .call("loadScript", &(path_str.as_ref(), plugin))
        .map_err(bus_err)?;
    if id < 0 {
        return Err(PalError::Backend(format!(
            "KWin would not load {}",
            path.display()
        )));
    }

    // Discard anything left over from a call that timed out.
    while s.reports.try_recv().is_ok() {}

    let ran = zbus::blocking::Proxy::new(
        &s.conn,
        KWIN,
        format!("/Scripting/Script{id}"),
        "org.kde.kwin.Script",
    )
    .and_then(|script| script.call::<_, _, ()>("run", &()))
    .map_err(bus_err);

    let report =
        ran.and_then(|()| {
            let deadline = std::time::Instant::now() + REPORT_TIMEOUT;
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match s.reports.recv_timeout(left) {
                    Ok((n, json)) if n == nonce => return Ok(json),
                    Ok(_) => continue, // someone else's, or forged
                    Err(_) => return Err(PalError::Backend(
                        "KWin script never reported back (see `journalctl --user -b | grep kwin`)"
                            .into(),
                    )),
                }
            }
        });

    // Unload whatever happened, so failed calls do not pile up scripts in KWin.
    let _: Result<bool, _> = scripting.call("unloadScript", &(plugin,));
    report
}

/// Where the script file goes. KWin has to be able to read it, so not `/tmp`:
/// the systemd unit gives actd a private one. `$XDG_RUNTIME_DIR/oracle` is the
/// unit's RuntimeDirectory, owned by the user KWin also runs as.
fn script_dir() -> Result<std::path::PathBuf, PalError> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| PalError::Backend("XDG_RUNTIME_DIR is not set".into()))?;
    let dir = base.join("oracle");
    if !dir.is_dir() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| PalError::Backend(format!("creating {}: {e}", dir.display())))?;
    }
    Ok(dir)
}

/// The listing script: normal windows, topmost first, reported to `me`.
fn list_script(me: &str, nonce: &str) -> String {
    // Both values are interpolated into JS string literals. A bus name and a
    // hex nonce cannot contain a quote, but encode them as JSON anyway so that
    // stays true by construction rather than by inspection.
    let me = serde_json::to_string(me).expect("string serializes");
    let nonce = serde_json::to_string(nonce).expect("string serializes");
    format!(
        r#"var out = [];
var ws = workspace.stackingOrder;
var desk = workspace.currentDesktop;
for (var i = ws.length - 1; i >= 0; i--) {{
  var w = ws[i];
  if (!w.normalWindow) continue;
  var here = w.onAllDesktops || w.desktops.some(function (d) {{ return d.id === desk.id; }});
  out.push({{
    id: w.internalId.toString(),
    title: w.caption,
    pid: w.pid,
    active: w === workspace.activeWindow,
    hidden: w.minimized || !here
  }});
}}
callDBus({me}, "{BRIDGE_PATH}", "{BRIDGE_IFACE}", "Report", {nonce}, JSON.stringify(out));
"#
    )
}

/// A stable u64 for a KWin UUID: its first 64 bits. v4 UUIDs are random, so a
/// collision needs two windows sharing 64 random bits.
fn wire_id(handle: &str) -> u64 {
    let hex: String = handle
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(16)
        .collect();
    u64::from_str_radix(&hex, 16).unwrap_or(0)
}

/// Image metadata KWin returns alongside the pipe.
#[derive(Debug, Default, PartialEq)]
struct Meta {
    width: u32,
    height: u32,
    stride: u32,
    format: u32,
}

fn capture(s: &Session, handle: Option<&str>) -> Result<(Meta, Vec<u8>), PalError> {
    let proxy = zbus::blocking::Proxy::new(
        &s.conn,
        KWIN,
        "/org/kde/KWin/ScreenShot2",
        "org.kde.KWin.ScreenShot2",
    )
    .map_err(bus_err)?;

    let (reader, writer) = std::io::pipe().map_err(|e| PalError::Backend(format!("pipe: {e}")))?;
    // KWin blocks writing into the pipe until someone reads, so read on a
    // thread that is already waiting by the time the reply arrives.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = Vec::new();
        let _ = tx.send(reader.read_to_end(&mut buf).map(|_| buf));
    });

    // No decoration (the title arrives separately and the frame adds nothing
    // for the model) and no cursor (it sits on top of exactly the text the
    // user is reading).
    let mut opts: HashMap<&str, Value> = HashMap::new();
    opts.insert("include-decoration", Value::from(false));
    opts.insert("include-cursor", Value::from(false));
    let fd = zbus::zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(writer));

    // The tuple owns our copy of the write end; it is dropped (closed) when
    // the call returns, which is what lets the reader see EOF.
    let reply: Result<HashMap<String, OwnedValue>, zbus::Error> = match handle {
        Some(h) => proxy.call("CaptureWindow", &(h, opts, fd)),
        None => proxy.call("CaptureActiveWindow", &(opts, fd)),
    };
    let reply = reply.map_err(capture_err)?;
    let meta = parse_meta(&reply)?;

    let pixels = rx
        .recv_timeout(PIXELS_TIMEOUT)
        .map_err(|_| PalError::Backend("KWin did not finish writing the capture".into()))?
        .map_err(|e| PalError::Backend(format!("reading capture: {e}")))?;
    Ok((meta, pixels))
}

/// Turn KWin's refusal into instructions; everything else passes through.
fn capture_err(e: zbus::Error) -> PalError {
    let text = e.to_string();
    if text.contains("NoAuthorized") {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "oracle-actd".into());
        return PalError::Backend(format!(
            "KWin refused the screenshot: no .desktop entry authorizes {exe}. \
             Run `scripts/setup.sh kwin` (it must point at this exact path)"
        ));
    }
    if text.contains("InvalidWindow") {
        return PalError::Backend("KWin: that window no longer exists".into());
    }
    bus_err(text)
}

fn parse_meta(reply: &HashMap<String, OwnedValue>) -> Result<Meta, PalError> {
    let get = |k: &str| -> Result<u32, PalError> {
        reply
            .get(k)
            .and_then(|v| u32::try_from(v).ok())
            .ok_or_else(|| PalError::Backend(format!("KWin capture reply has no `{k}`")))
    };
    if let Some(kind) = reply.get("type").and_then(|v| <&str>::try_from(v).ok()) {
        if kind != "raw" {
            return Err(PalError::Backend(format!(
                "KWin returned a `{kind}` capture; only raw is understood"
            )));
        }
    }
    Ok(Meta {
        width: get("width")?,
        height: get("height")?,
        stride: get("stride")?,
        format: get("format")?,
    })
}

// QImage::Format values KWin produces.
const QIMAGE_RGB32: u32 = 4;
const QIMAGE_ARGB32: u32 = 5;
const QIMAGE_ARGB32_PREMULTIPLIED: u32 = 6;

/// Convert a KWin raw capture to tightly packed RGBA.
///
/// The three 32-bit QImage formats are all `0xAARRGGBB` words in native
/// (little-endian) order, i.e. B, G, R, A in memory. Rows may be padded, so
/// the stride is honoured rather than assumed.
fn to_rgba(meta: &Meta, src: &[u8]) -> Result<(Vec<u8>, u32, u32), PalError> {
    let Meta {
        width,
        height,
        stride,
        format,
    } = *meta;
    if !matches!(
        format,
        QIMAGE_RGB32 | QIMAGE_ARGB32 | QIMAGE_ARGB32_PREMULTIPLIED
    ) {
        return Err(PalError::Backend(format!(
            "KWin capture is in QImage format {format}, which this backend does not convert"
        )));
    }
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if w == 0 || h == 0 {
        return Err(PalError::Backend(format!(
            "window has no area ({width}x{height}); it is probably minimized"
        )));
    }
    if stride < w * 4 || src.len() < stride * (h - 1) + w * 4 {
        return Err(PalError::Backend(format!(
            "capture is {} bytes, too short for {width}x{height} at stride {stride}",
            src.len()
        )));
    }

    let mut out = Vec::with_capacity(w * h * 4);
    for row in src.chunks(stride).take(h) {
        for px in row[..w * 4].as_chunks::<4>().0 {
            let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
            let (r, g, b, a) = match format {
                QIMAGE_RGB32 => (r, g, b, 255),
                QIMAGE_ARGB32_PREMULTIPLIED if a != 0 && a != 255 => {
                    let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                    (un(r), un(g), un(b), a)
                }
                _ => (r, g, b, a),
            };
            out.extend_from_slice(&[r, g, b, a]);
        }
    }
    Ok((out, width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_id_is_the_uuid_head_and_ignores_punctuation() {
        assert_eq!(
            wire_id("{35bbf44e-dcf6-4da8-ae47-4f30a2cf6889}"),
            0x35bb_f44e_dcf6_4da8
        );
        assert_ne!(
            wire_id("{35bbf44e-dcf6-4da8-ae47-4f30a2cf6889}"),
            wire_id("{671219d2-e88f-47c1-a626-66d6c5b213c6}")
        );
    }

    #[test]
    fn the_script_reports_to_us_with_the_nonce_and_the_exact_method_name() {
        let js = list_script(":1.234", "abc123");
        assert!(js.contains(
            r#"callDBus(":1.234", "/dev/oracle/kwin", "dev.oracle.KwinBridge", "Report", "abc123""#
        ));
        // Topmost first: walk the stacking order backwards.
        assert!(js.contains("ws.length - 1; i >= 0; i--"));
    }

    #[test]
    fn a_hostile_bus_name_cannot_break_out_of_the_string_literal() {
        let js = list_script("\");evil();(\"", "n");
        assert!(js.contains(r#"callDBus("\");evil();(\"""#));
    }

    #[test]
    fn a_report_parses_into_windows() {
        let json = r#"[{"id":"{a}","title":"docs.rs","pid":7,"active":true,"hidden":false},
                       {"id":"{b}","title":"Spotify","pid":8,"active":false,"hidden":true}]"#;
        let ws: Vec<KwinWindow> = serde_json::from_str(json).unwrap();
        assert_eq!(ws.len(), 2);
        assert!(ws[0].active && !ws[0].hidden);
        assert!(ws[1].hidden);
    }

    fn meta(w: u32, h: u32, stride: u32, format: u32) -> Meta {
        Meta {
            width: w,
            height: h,
            stride,
            format,
        }
    }

    #[test]
    fn bgra_words_become_rgba_and_row_padding_is_skipped() {
        // 1x2 image, stride 8: one pixel per row plus 4 bytes of padding.
        let src = [
            10, 20, 30, 255, 0xEE, 0xEE, 0xEE, 0xEE, // row 0: B G R A + pad
            1, 2, 3, 255, 0xEE, 0xEE, 0xEE, 0xEE, // row 1
        ];
        let (rgba, w, h) = to_rgba(&meta(1, 2, 8, QIMAGE_ARGB32_PREMULTIPLIED), &src).unwrap();
        assert_eq!((w, h), (1, 2));
        assert_eq!(rgba, vec![30, 20, 10, 255, 3, 2, 1, 255]);
    }

    #[test]
    fn rgb32_ignores_the_padding_byte_and_is_opaque() {
        let (rgba, ..) = to_rgba(&meta(1, 1, 4, QIMAGE_RGB32), &[10, 20, 30, 0]).unwrap();
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn premultiplied_alpha_is_undone() {
        // 50% alpha, colour premultiplied from (200, 100, 0).
        let (rgba, ..) = to_rgba(
            &meta(1, 1, 4, QIMAGE_ARGB32_PREMULTIPLIED),
            &[0, 50, 100, 128],
        )
        .unwrap();
        assert_eq!(rgba, vec![199, 100, 0, 128]);
    }

    #[test]
    fn a_short_buffer_or_unknown_format_is_an_error_not_a_panic() {
        assert!(to_rgba(&meta(2, 2, 8, QIMAGE_ARGB32), &[0; 12]).is_err());
        assert!(to_rgba(&meta(1, 1, 4, 13), &[0; 4]).is_err());
        assert!(to_rgba(&meta(0, 1, 4, QIMAGE_ARGB32), &[0; 4]).is_err());
    }

    #[test]
    fn the_last_row_need_not_carry_padding() {
        // Some producers trim the final row's padding; that must still convert.
        let src = [1, 2, 3, 255, 0, 0, 0, 0, 4, 5, 6, 255];
        let (rgba, ..) = to_rgba(&meta(1, 2, 8, QIMAGE_ARGB32), &src).unwrap();
        assert_eq!(rgba, vec![3, 2, 1, 255, 6, 5, 4, 255]);
    }

    #[test]
    fn meta_is_read_from_the_reply() {
        let mut r: HashMap<String, OwnedValue> = HashMap::new();
        for (k, v) in [
            ("width", 3u32),
            ("height", 2),
            ("stride", 12),
            ("format", 6),
        ] {
            r.insert(k.into(), OwnedValue::from(v));
        }
        r.insert(
            "type".into(),
            OwnedValue::try_from(Value::from("raw")).unwrap(),
        );
        assert_eq!(parse_meta(&r).unwrap(), meta(3, 2, 12, 6));
        r.remove("stride");
        assert!(parse_meta(&r).is_err());
    }

    /// Against the real compositor. Run on a Plasma session with
    /// `cargo test -p oracle-actd -- --ignored kwin_live`.
    #[test]
    #[ignore]
    fn kwin_live_lists_and_captures_the_topmost_window() {
        let k = Kwin::default();
        let ws = k.list_windows().expect("list");
        assert!(!ws.is_empty(), "a desktop session has windows");
        let target = ws.iter().find(|w| !w.minimized).expect("a visible window");
        let img = k.capture_window(Some(target.id), 1024).expect(
            "capture (needs the .desktop entry for the test binary, see scripts/setup.sh kwin)",
        );
        assert!(img.width <= 1024 && img.width > 0 && img.height > 0);
        assert_eq!(img.title, target.title);
    }
}
