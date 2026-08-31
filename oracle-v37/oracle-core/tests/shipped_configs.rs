//! The profiles under `deploy/` must actually load.
//!
//! `Config` is `deny_unknown_fields`, so a key added to the struct but spelled
//! differently in the shipped TOML — or a key left in the TOML after a rename —
//! fails at *startup*, on the user's machine, with the assistant not coming up.
//! Nothing covered these files before, which made every config change a live
//! test. This is the cheapest possible guard.

use oracle_core::config::Config;
use std::path::PathBuf;

fn deploy(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("oracle-core has a parent")
        .join("deploy")
        .join(name)
}

#[test]
fn the_windows_profile_loads_and_validates() {
    let path = deploy("oracle.windows.toml");
    let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    cfg.validate()
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
}

#[test]
fn the_windows_profile_carries_a_small_tier_on_its_own_port() {
    let cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    // Shipped off — it needs a model download first — but present and correct,
    // so turning it on is a one-word edit rather than a research project.
    assert!(!cfg.llm.small.enabled, "ship the tier off by default");
    assert!(
        cfg.llm.small.backend.contains("8081"),
        "the small tier needs its own port, got {:?}",
        cfg.llm.small.backend
    );
    assert_ne!(cfg.llm.small.backend, cfg.llm.backend);
    assert!(
        cfg.llm.small.resident,
        "the small tier is resident by design"
    );
}

#[test]
fn the_windows_profile_carries_an_embedder_on_a_third_port() {
    let cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    assert!(!cfg.memory.embedder.enabled, "ship it off by default");
    // Three servers, three ports. Any two sharing one is a silent failure: the
    // embedder would post to a chat model and get prose back.
    let ports = [
        cfg.llm.backend.clone(),
        cfg.llm.small.backend.clone(),
        cfg.memory.embedder.backend.clone(),
    ];
    let mut unique = ports.clone().to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), 3, "each server needs its own port: {ports:?}");
    assert_eq!(cfg.memory.embedder.dim, 384, "BGE-small is 384-d");
}

#[test]
fn the_shipped_embedder_args_ask_for_mean_pooling() {
    // BGE without --pooling mean produces vectors that are subtly wrong rather
    // than absent -- retrieval keeps working and keeps being mediocre, which is
    // very hard to attribute back to a missing flag.
    let cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    let args = cfg.supervise.embedder_args.join(" ");
    assert!(args.contains("--embedding"), "got: {args}");
    assert!(args.contains("--pooling mean"), "got: {args}");
}

#[test]
fn enabling_the_shipped_embedder_still_validates() {
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.memory.embedder.enabled = true;
    cfg.supervise.autostart_embedder = true;
    cfg.validate()
        .expect("the shipped embedder values must validate once enabled");
}

#[test]
fn the_ambient_index_ships_off() {
    let cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    assert!(!cfg.ambient.enabled, "never on by default");
    assert!(
        cfg.ambient.retain_days > 0,
        "a default of 0 would keep screen observations forever"
    );
}

#[test]
fn ambient_without_a_vision_tier_is_rejected() {
    // The failure this prevents: capturing the screen every 45 seconds and
    // queueing it for a model that does not exist -- all of the privacy cost,
    // none of the benefit, and no error anywhere.
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.ambient.enabled = true;
    cfg.llm.small.enabled = false;
    let err = cfg.validate().expect_err("must not validate");
    assert!(format!("{err}").contains("vision tier"), "{err}");
}

#[test]
fn enabling_the_shipped_ambient_index_with_its_tier_validates() {
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.ambient.enabled = true;
    cfg.llm.small.enabled = true;
    cfg.validate()
        .expect("the shipped ambient values must validate");
}

#[test]
fn consolidation_ships_off_but_ready() {
    let cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    assert!(!cfg.consolidate.enabled);
    assert!(
        cfg.consolidate.from_observations,
        "the observation source is the reason the pass exists"
    );
}

#[test]
fn consolidation_without_a_small_tier_is_rejected() {
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.consolidate.enabled = true;
    cfg.llm.small.enabled = false;
    let err = cfg.validate().expect_err("must not validate");
    assert!(format!("{err}").contains("small tier"), "{err}");
}

#[test]
fn the_full_stack_validates_when_every_piece_is_enabled() {
    // The configuration the user actually wants: both tiers, semantic
    // embeddings, the ambient index, and consolidation promoting what it sees
    // before retention sweeps it. If these cannot be on together, none of it
    // works as designed.
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.llm.small.enabled = true;
    cfg.supervise.autostart_small_llm = true;
    cfg.memory.embedder.enabled = true;
    cfg.supervise.autostart_embedder = true;
    cfg.ambient.enabled = true;
    cfg.consolidate.enabled = true;
    cfg.validate()
        .expect("the whole stack must validate together");
}

#[test]
fn the_ambient_profile_loads_with_everything_switched_on() {
    // The file the user actually copies to %APPDATA% when testing the new
    // stack. It must validate as written -- a config error here surfaces as an
    // assistant that will not start, after a long release build.
    let path = deploy("oracle.windows.ambient.toml");
    let cfg = Config::load(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(cfg.llm.small.enabled, "the vision tier must be on");
    assert!(cfg.memory.embedder.enabled, "semantic recall must be on");
    assert!(cfg.ambient.enabled, "the ambient index must be on");
    assert!(cfg.consolidate.enabled, "consolidation must be on");
    assert!(cfg.supervise.autostart_small_llm);
    assert!(cfg.supervise.autostart_embedder);
}

#[test]
fn the_ambient_profile_uses_three_distinct_ports() {
    let cfg = Config::load(&deploy("oracle.windows.ambient.toml")).expect("loads");
    let mut ports = vec![
        cfg.llm.backend.clone(),
        cfg.llm.small.backend.clone(),
        cfg.memory.embedder.backend.clone(),
    ];
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), 3, "each llama.cpp server needs its own port");
}

#[test]
fn the_ambient_profile_asks_for_a_vision_projector() {
    // A VLM loaded without its --mmproj starts cleanly and is simply blind.
    // Every frame then comes back as "nothing legible" and the index looks
    // switched on while indexing nothing.
    let cfg = Config::load(&deploy("oracle.windows.ambient.toml")).expect("loads");
    let args = cfg.supervise.small_llm_args.join(" ");
    assert!(args.contains("--mmproj"), "got: {args}");
}

#[test]
fn the_two_windows_profiles_differ_only_in_what_is_switched_on() {
    // The ambient profile is a derivative, not a fork: device names, paths and
    // ports must not drift apart from the base profile as either is edited.
    let base = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    let amb = Config::load(&deploy("oracle.windows.ambient.toml")).expect("loads");
    assert_eq!(base.memory.db_path, amb.memory.db_path);
    assert_eq!(base.general.runtime_dir, amb.general.runtime_dir);
    assert_eq!(base.audio.input_device, amb.audio.input_device);
    assert_eq!(base.llm.backend, amb.llm.backend);
    assert_eq!(base.llm.small.backend, amb.llm.small.backend);
    assert_eq!(base.memory.embedder.backend, amb.memory.embedder.backend);
    assert_eq!(base.supervise.small_llm_args, amb.supervise.small_llm_args);
    assert_eq!(base.supervise.embedder_args, amb.supervise.embedder_args);
}

#[test]
fn enabling_the_shipped_small_tier_still_validates() {
    // The interesting case: the user flips `enabled = true` and nothing else.
    // Every small-tier rule must pass on the values as shipped, or that edit
    // fails at startup with a config error.
    let mut cfg = Config::load(&deploy("oracle.windows.toml")).expect("loads");
    cfg.llm.small.enabled = true;
    cfg.supervise.autostart_small_llm = true;
    cfg.validate()
        .expect("the shipped small-tier values must validate once enabled");
}
