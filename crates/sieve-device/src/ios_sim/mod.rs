//! iOS Simulator setup (macOS only): `simctl` root cert install and the Mac
//! system proxy via `networksetup`, with saved state for restore.
//!
//! Every subprocess goes through [`CommandRunner`] so tests can mock it.

mod runner;
mod simctl;
mod sysproxy;

pub use runner::{CommandRunner, SystemRunner};
pub use simctl::{Simulator, booted_simulators, install_ca, parse_booted};
pub use sysproxy::{
    LeftoverState, ProxySettings, SavedState, active_service, apply, find_leftover,
    parse_proxy_settings, parse_service_for_device, restore, run_guarded, state_file,
};
