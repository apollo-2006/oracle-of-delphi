//! Making a shipped config work on a machine that is not the author's.
//!
//! Every path in `oracle.toml` used to be absolute and machine-specific:
//! `C:\Users\apollo\...` in the Windows profile, `/Users/abirdeol/...` in the
//! macOS one. That is fine for the person who wrote it and useless to everyone
//! else — a fresh clone on a second Windows box failed exactly as hard as one
//! on a Mac, and the failure was a supervised child that restart-loops in a log
//! file rather than anything that names the wrong path.
//!
//! So paths may now be written relative to the checkout:
//!
//! ```toml
//! llm_program = "${ORACLE_ROOT}/llama.cpp/build/bin/llama-server"
//! db_path     = "~/Library/Application Support/oracle/oracle.db"
//! ```
//!
//! `${ORACLE_ROOT}` is the repository root, discovered automatically (see
//! [`oracle_root`]). `~` is the user's home. Any other `${VAR}` or `$VAR` comes
//! from the environment, which is what lets one profile serve `%APPDATA%` on
//! Windows and `$XDG_CONFIG_HOME` elsewhere.
//!
//! Expansion happens once, at config load, so everything downstream — the
//! supervisor, the voice pipeline, the memory store — keeps seeing plain
//! absolute paths and needs no knowledge of any of this.

use std::path::{Path, PathBuf};

/// The filename that marks the repository root.
///
/// A marker file rather than a heuristic: looking for `.git` breaks in a
/// tarball or a vendored copy, and looking for a directory named `oracle-v37`
/// breaks the moment that directory is renamed. An empty marker file is
/// explicit and survives both.
pub const ROOT_MARKER: &str = ".oracle-root";

/// Locate the repository root, or `None` if this is not running from a checkout.
///
/// In order:
/// 1. `$ORACLE_ROOT`, if set — always wins, so a packaged install or an unusual
///    layout can simply state the answer.
/// 2. Upward from the config file, if one was given.
/// 3. Upward from this executable. Covers `target/{debug,release}/oracle-core`
///    and a binary sitting next to the checkout, and is the branch that works
///    when the config has been copied out to `%APPDATA%` or
///    `~/Library/Application Support`, which the docs tell users to do.
pub fn oracle_root(config_path: Option<&Path>) -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var("ORACLE_ROOT") {
        let explicit = explicit.trim();
        if !explicit.is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    if let Some(cfg) = config_path {
        // A relative --config is resolved against the cwd first, or `parent()`
        // of a bare filename is "" and the walk below inspects nothing.
        let start = if cfg.is_absolute() {
            cfg.to_path_buf()
        } else {
            std::env::current_dir().ok()?.join(cfg)
        };
        if let Some(found) = ascend_for_marker(start.parent()?) {
            return Some(found);
        }
    }
    let exe = std::env::current_exe().ok()?;
    ascend_for_marker(exe.parent()?)
}

/// Walk up from `start` looking for [`ROOT_MARKER`].
fn ascend_for_marker(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(ROOT_MARKER).exists() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// Expand `~`, `${ORACLE_ROOT}` and environment variables in one path.
///
/// An unknown variable is left **literally in place** rather than expanded to
/// nothing. Both fail, but `${WHISPER_DIR}/whisper-cli: not found` says what is
/// missing, where `/whisper-cli: not found` says only that something is.
pub fn expand(raw: &str, root: Option<&Path>) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut out = expand_vars(raw, root);

    // `~` only at the start, and only as a whole component: a path may
    // legitimately contain one elsewhere (Windows 8.3 names like PROGRA~1).
    if out == "~" || out.starts_with("~/") || out.starts_with("~\\") {
        if let Some(home) = home_dir() {
            let rest = &out[1..];
            let rest = rest.strip_prefix(['/', '\\']).unwrap_or(rest);
            let mut p = home;
            if !rest.is_empty() {
                p.push(rest);
            }
            out = p.to_string_lossy().into_owned();
        }
    }
    out
}

/// The `<os>-<arch>` directory name this build's binaries live under.
///
/// Lets one shipped profile serve every machine of its family: an Apple Silicon
/// and an Intel Mac read the same `oracle.macos.toml` and resolve
/// `whisper/${ORACLE_PLATFORM}/whisper-cli` to different directories, instead of
/// needing a fourth profile or a hand-edit after cloning.
///
/// Matches the names `scripts/setup.sh` and `scripts/setup.ps1` install into.
pub fn platform() -> &'static str {
    let os = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    // A tiny match beats formatting into a String: this is a &'static str, so
    // callers never allocate to read it.
    match (os, arch) {
        ("macos", "arm64") => "macos-arm64",
        ("macos", _) => "macos-x64",
        ("windows", "arm64") => "windows-arm64",
        ("windows", _) => "windows-x64",
        (_, "arm64") => "linux-arm64",
        _ => "linux-x64",
    }
}

/// `$HOME`, or `%USERPROFILE%` on Windows where `HOME` is often unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .filter(|h| !h.trim().is_empty())
        .map(PathBuf::from)
}

/// Substitute `${VAR}` and `$VAR`. `ORACLE_ROOT` resolves to `root` even when it
/// is absent from the environment, which is the whole point of discovering it.
fn expand_vars(raw: &str, root: Option<&Path>) -> String {
    let lookup = |name: &str| -> Option<String> {
        if name == "ORACLE_ROOT" {
            if let Some(r) = root {
                return Some(r.to_string_lossy().into_owned());
            }
        }
        if name == "ORACLE_PLATFORM" {
            return Some(platform().to_string());
        }
        std::env::var(name).ok()
    };

    let mut out = String::with_capacity(raw.len());
    let bytes: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != '$' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        // ${NAME}
        if i + 1 < bytes.len() && bytes[i + 1] == '{' {
            if let Some(close) = (i + 2..bytes.len()).find(|&j| bytes[j] == '}') {
                let name: String = bytes[i + 2..close].iter().collect();
                match lookup(&name) {
                    Some(v) => out.push_str(&v),
                    // Unknown: keep it visible in the resulting error.
                    None => out.push_str(&format!("${{{name}}}")),
                }
                i = close + 1;
                continue;
            }
        }
        // $NAME — letters, digits and underscore.
        let start = i + 1;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == '_') {
            end += 1;
        }
        if end > start {
            let name: String = bytes[start..end].iter().collect();
            match lookup(&name) {
                Some(v) => out.push_str(&v),
                None => {
                    out.push('$');
                    out.push_str(&name);
                }
            }
            i = end;
            continue;
        }
        // A bare '$' is just a character.
        out.push('$');
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_root_expands_even_when_the_env_var_is_unset() {
        let root = PathBuf::from("/checkout");
        let got = expand("${ORACLE_ROOT}/piper/piper", Some(&root));
        assert_eq!(got, "/checkout/piper/piper");
        // The bare form too, since shell habits differ.
        assert_eq!(expand("$ORACLE_ROOT/x", Some(&root)), "/checkout/x");
    }

    #[test]
    fn an_unknown_variable_is_left_visible_rather_than_blanked() {
        // Expanding to "" would turn "${WHISPER}/whisper-cli" into
        // "/whisper-cli" -- a path that says nothing about what was missing.
        let got = expand("${NOT_SET_ANYWHERE_XYZ}/whisper-cli", None);
        assert_eq!(got, "${NOT_SET_ANYWHERE_XYZ}/whisper-cli");
        assert_eq!(
            expand("$NOT_SET_ANYWHERE_XYZ/w", None),
            "$NOT_SET_ANYWHERE_XYZ/w"
        );
    }

    #[test]
    fn tilde_expands_only_as_a_leading_component() {
        let home = home_dir().expect("a home directory");
        let got = expand("~/oracle.db", None);
        // Join the way expand() does rather than with a literal '/': on Windows
        // PathBuf::push uses a backslash, so a hardcoded separator here fails
        // against correct behaviour.
        assert_eq!(got, home.join("oracle.db").to_string_lossy());
        // Not in the middle: Windows 8.3 names such as PROGRA~1 are real paths.
        assert_eq!(expand("/opt/PROGRA~1/x", None), "/opt/PROGRA~1/x");
    }

    #[test]
    fn platform_expands_to_this_builds_directory_name() {
        let got = expand("whisper/${ORACLE_PLATFORM}/whisper-cli", None);
        assert_eq!(got, format!("whisper/{}/whisper-cli", platform()));
        // Shape, not a hardcoded value: this test must pass on every target.
        let p = platform();
        assert!(p.contains('-'), "expected <os>-<arch>, got {p}");
        let (os, arch) = p.split_once('-').unwrap();
        assert!(matches!(os, "macos" | "windows" | "linux"), "{os}");
        assert!(matches!(arch, "arm64" | "x64"), "{arch}");
    }

    #[test]
    fn platform_matches_the_host_this_test_runs_on() {
        if cfg!(target_os = "macos") {
            assert!(platform().starts_with("macos-"), "{}", platform());
        }
        if cfg!(target_arch = "aarch64") {
            assert!(platform().ends_with("-arm64"), "{}", platform());
        }
    }

    #[test]
    fn a_path_with_no_variables_is_returned_unchanged() {
        assert_eq!(
            expand("/tmp/oracle/actd.sock", None),
            "/tmp/oracle/actd.sock"
        );
        assert_eq!(expand("", None), "");
        // A lone '$' is a character, not a broken variable.
        assert_eq!(expand("/tmp/a$b", None), "/tmp/a$b");
    }

    #[test]
    fn windows_style_paths_survive_expansion() {
        let root = PathBuf::from(r"C:\checkout");
        let got = expand(r"${ORACLE_ROOT}\piper\piper.exe", Some(&root));
        assert_eq!(got, r"C:\checkout\piper\piper.exe");
    }

    #[test]
    fn the_marker_is_found_by_walking_up() {
        let dir = std::env::temp_dir().join(format!("oracle-root-{}", std::process::id()));
        let deep = dir.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(dir.join(ROOT_MARKER), "").unwrap();
        assert_eq!(ascend_for_marker(&deep), Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_marker_anywhere_is_none_not_a_wrong_guess() {
        // Better to leave ${ORACLE_ROOT} unexpanded and fail with the variable
        // named than to silently pick "/" and build nonsense paths from it.
        assert_eq!(ascend_for_marker(std::path::Path::new("/")), None);
    }
}
