fn main() {
    // The HUD (oracle-hud/dist) is embedded into this binary by rust-embed. Tell
    // cargo to rebuild — and thus re-embed — whenever that built frontend
    // changes, so `cargo build` after `npm run build` always ships the latest HUD.
    println!("cargo:rerun-if-changed=../oracle-hud/dist");
}
