//! Which window counts as "the user's screen".
//!
//! Three separate call sites need the same answer — the prompt's window-title
//! block, the `os.list_windows` tool, and the ambient sampler — and they had
//! independently grown two copies of the rule between them. The third consumer
//! was written without it, which is how the ambient index ended up
//! photographing Pythia's own HUD and storing it as a memory of what the user
//! was looking at.
//!
//! One predicate, one place. A future consumer that forgets to call it is a
//! visible omission rather than a silently different answer.

/// Whether a window is Pythia's own UI.
///
/// Matched on the title because that is all the PAL reports in common across
/// three platforms. It is a substring match on purpose: the HUD's title varies
/// ("Oracle of Delphi", "Oracle of Delphi — listening") and a prefix test would
/// miss the variants.
pub fn is_own_window(title: &str) -> bool {
    title.to_lowercase().contains("oracle of delphi")
}

/// Whether a window is something the user could actually be looking at.
///
/// Excludes our own UI, untitled windows (tool windows, invisible hosts), and
/// minimized ones — a minimized window is real enough to focus or restore, but
/// it is not on screen, and reading it back as context is how the assistant
/// describes something the user cannot see.
pub fn is_user_facing(title: &str, minimized: bool) -> bool {
    let t = title.trim();
    !t.is_empty() && !minimized && !is_own_window(t)
}

/// Pick the window the user is actually looking at from an actd `ListWindows`
/// payload, returning `(id, title)`.
///
/// Windows arrive in z-order, so the first user-facing entry is the frontmost
/// thing that is not us. Resolving an explicit id — rather than letting the
/// backend take the foreground window — is what keeps our own HUD out of the
/// frame when the user has just been talking to it.
pub fn pick_target(windows: &[serde_json::Value]) -> Option<(u64, String)> {
    windows.iter().find_map(|w| {
        let title = w.get("title")?.as_str()?.trim();
        let minimized = w
            .get("minimized")
            .and_then(|m| m.as_bool())
            .unwrap_or(false);
        if !is_user_facing(title, minimized) {
            return None;
        }
        Some((w.get("id")?.as_u64()?, title.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn win(id: u64, title: &str, minimized: bool) -> serde_json::Value {
        json!({ "id": id, "title": title, "minimized": minimized, "pid": 1, "focused": false })
    }

    #[test]
    fn our_own_window_is_recognized_in_its_variants() {
        assert!(is_own_window("Oracle of Delphi"));
        assert!(is_own_window("oracle of delphi — listening"));
        assert!(is_own_window("Oracle of Delphi (2)"));
        assert!(!is_own_window("Oracle SQL Developer"));
        assert!(!is_own_window("docs.rs — tokio"));
    }

    #[test]
    fn a_minimized_or_untitled_window_is_not_the_screen() {
        assert!(!is_user_facing("Spotify", true));
        assert!(!is_user_facing("", false));
        assert!(!is_user_facing("   ", false));
        assert!(is_user_facing("Spotify", false));
    }

    #[test]
    fn the_target_is_the_frontmost_window_that_is_not_us() {
        // The exact case that broke the ambient index: the user has just
        // spoken, so the HUD is frontmost, and the thing they were reading is
        // directly behind it.
        let ws = vec![
            win(1, "Oracle of Delphi", false),
            win(2, "docs.rs — tokio", false),
            win(3, "Discord", false),
        ];
        assert_eq!(pick_target(&ws), Some((2, "docs.rs — tokio".to_string())));
    }

    #[test]
    fn minimized_and_untitled_windows_are_skipped_in_z_order() {
        let ws = vec![
            win(1, "", false),
            win(2, "Spotify", true),
            win(3, "main.rs", false),
        ];
        assert_eq!(pick_target(&ws), Some((3, "main.rs".to_string())));
    }

    #[test]
    fn a_desktop_with_nothing_but_us_has_no_target() {
        // Must be None, not a fallback to our own window: capturing the HUD is
        // the failure this module exists to prevent.
        let ws = vec![win(1, "Oracle of Delphi", false)];
        assert_eq!(pick_target(&ws), None);
        assert_eq!(pick_target(&[]), None);
    }

    #[test]
    fn a_malformed_entry_is_skipped_rather_than_panicking() {
        let ws = vec![json!({ "title": "no id here" }), win(7, "Real", false)];
        assert_eq!(pick_target(&ws), Some((7, "Real".to_string())));
    }
}
