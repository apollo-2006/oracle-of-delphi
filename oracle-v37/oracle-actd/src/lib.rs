//! `oracle-actd`: the privileged actuator daemon, as a library so integration
//! tests can drive the real socket server. The binary (`main.rs`) is a thin
//! wrapper that constructs a [`daemon::Daemon`] over the platform backend and
//! runs [`server::serve`].

pub mod audit;
pub mod daemon;
pub mod pal;
pub mod policy;
pub mod sandbox;
pub mod server;
