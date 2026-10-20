//! Android mode 2: inject the sieve CA into the system trust store on
//! emulators and rooted devices. Implemented in phase 4.

use anyhow::{Result, bail};

use super::AndroidBridge;

/// Injects `ca_pem` as `<subject_hash_old>.0` into the device's system CA
/// store, for both the legacy `/system` store and the Android 14+ APEX store.
/// The change does not survive a reboot.
pub async fn inject<B: AndroidBridge>(
    _bridge: &B,
    _serial: Option<&str>,
    _ca_pem: &str,
    _subject_hash_old: &str,
) -> Result<()> {
    bail!("system CA injection is not implemented yet (phase 4)")
}
