//! Android setup. Mode 1 (no root, user CA + `http_proxy`) lives in [`mode1`];
//! mode 2 (system CA injection on emulators or rooted devices) in
//! [`system_ca`]; the Frida certificate-pinning bypass workflow in [`frida`].

pub mod bridge;
pub mod frida;
pub mod mode1;
pub mod system_ca;

pub use bridge::{Adb, AndroidBridge};
