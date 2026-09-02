//! The shipped profiles must resolve to real paths on the machine running them.
//!
//! `check-config` proves a profile parses and validates. It does not prove that
//! `${ORACLE_ROOT}` found the checkout, or that the file it points at exists —
//! and a path that expands wrongly fails much later, as a supervised child
//! restart-looping in a log nobody is watching.

use oracle_core::config::Config;
use std::path::{Path, PathBuf};

fn deploy(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("oracle-core has a parent")
        .join("deploy")
        .join(name)
}

/// The profile for the platform this test is running on, or None if we do not
/// ship one (so the suite stays green on Linux/CI rather than failing for a
/// file that was never meant to exist).
fn profile_for_host() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(deploy("oracle.macos.toml"))
    } else if cfg!(target_os = "windows") {
        Some(deploy("oracle.windows.toml"))
    } else {
        None
    }
}

#[test]
fn the_host_profile_leaves_no_variable_unexpanded() {
    let Some(path) = profile_for_host() else {
        return;
    };
    let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

    // An unknown variable is deliberately left literal so it shows up in an
    // error message. That is the right behaviour at run time and a bug in a
    // shipped profile: it means the profile names something we do not define.
    let fields: Vec<(&str, String)> = vec![
        ("general.runtime_dir", cfg.general.runtime_dir.clone()),
        ("memory.db_path", cfg.memory.db_path.clone()),
        ("llm.model_dir", cfg.llm.model_dir.clone()),
        ("actd.socket", cfg.actd.socket.clone()),
        ("voice.tts_program", cfg.voice.tts_program.clone()),
        ("voice.stt_program", cfg.voice.stt_program.clone()),
        ("voice.wake_program", cfg.voice.wake_program.clone()),
        ("supervise.llm_program", cfg.supervise.llm_program.clone()),
    ];
    let mut all: Vec<(String, String)> = fields
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    for (label, args) in [
        ("voice.tts_args", &cfg.voice.tts_args),
        ("voice.stt_args", &cfg.voice.stt_args),
        ("voice.wake_args", &cfg.voice.wake_args),
        ("supervise.llm_args", &cfg.supervise.llm_args),
        ("supervise.small_llm_args", &cfg.supervise.small_llm_args),
        ("supervise.embedder_args", &cfg.supervise.embedder_args),
    ] {
        for (i, a) in args.iter().enumerate() {
            all.push((format!("{label}[{i}]"), a.clone()));
        }
    }

    for (field, value) in &all {
        assert!(
            !value.contains("${") && !value.contains("$ORACLE"),
            "{field} still contains an unexpanded variable: {value}"
        );
        assert!(
            !value.starts_with('~'),
            "{field} still starts with an unexpanded ~: {value}"
        );
    }
}

#[test]
fn the_host_profile_points_at_files_that_exist() {
    let Some(path) = profile_for_host() else {
        return;
    };
    let cfg = Config::load(&path).expect("loads");

    // Only paths inside the checkout are asserted. Model GGUFs and the user's
    // Google credentials are deliberate downloads, not something a clone has,
    // and asserting on them would make this test a nag rather than a check.
    let root = oracle_core::paths::oracle_root(Some(&path))
        .expect("the .oracle-root marker must be findable from deploy/");
    assert!(root.join(".oracle-root").exists(), "{root:?}");

    let inside_checkout: Vec<(&str, &str)> = [
        ("voice.tts_program", cfg.voice.tts_program.as_str()),
        ("voice.stt_program", cfg.voice.stt_program.as_str()),
        ("voice.wake_program", cfg.voice.wake_program.as_str()),
    ]
    .into_iter()
    .filter(|(_, p)| !p.is_empty() && Path::new(p).starts_with(&root))
    .collect();

    let present = inside_checkout
        .iter()
        .filter(|(_, p)| Path::new(p).exists())
        .count();

    // A machine where `scripts/setup.sh` has never run — a clean CI runner, or
    // a fresh clone — has none of these, and that is not a failure: the setup
    // script is a documented separate step, and making the suite depend on a
    // network fetch would be worse than the check is worth.
    //
    // The moment ANY of them exists the checkout is set up, and a missing
    // sibling then means the profile points somewhere wrong. That is the bug
    // worth catching, and it is exactly what a hand-edited path looks like.
    if present == 0 {
        eprintln!(
            "skipping: scripts/setup.sh has not been run in {} — nothing to check",
            root.display()
        );
        return;
    }

    let missing: Vec<String> = inside_checkout
        .iter()
        .filter(|(_, p)| !Path::new(p).exists())
        .map(|(label, p)| format!("{label}: {p}"))
        .collect();

    assert!(
        missing.is_empty(),
        "this checkout is set up ({present} of {} voice programs present), but the \
         profile names files that do not exist. Re-run scripts/setup.sh, or fix the \
         path. Missing:\n  {}",
        inside_checkout.len(),
        missing.join("\n  ")
    );
}
