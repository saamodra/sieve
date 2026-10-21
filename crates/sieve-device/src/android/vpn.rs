//! Android VPN mode: the `android-vpn/` companion app captures all TCP on the
//! device and forwards it to sieve through `adb reverse`. This module does the
//! host side: the reverse, the CA push, and launching the app.

use anyhow::{Context, Result};

use super::{
    bridge::AndroidBridge,
    mode1::{DEVICE_CA_PATH, push_ca, tcp},
};

/// Application id of the companion app.
pub const APP_ID: &str = "dev.sieve.vpn";
const MAIN_ACTIVITY: &str = "dev.sieve.vpn/.MainActivity";

/// Makes the proxy reachable at `127.0.0.1:<port>` on the device and pushes the
/// CA. Changes no device setting: the VPN is only active while the app runs it.
pub async fn setup<B: AndroidBridge>(
    bridge: &B,
    serial: &str,
    port: u16,
    ca_pem: &str,
) -> Result<()> {
    let t = tcp(port);
    bridge
        .adb(Some(serial), &["reverse", &t, &t])
        .await
        .context("adb reverse")?;
    push_ca(bridge, serial, ca_pem).await
}

/// Opens the companion app, pointed at the proxy. The first start still needs
/// the user to accept Android's VPN consent dialog.
pub async fn launch<B: AndroidBridge>(bridge: &B, serial: &str, port: u16) -> Result<()> {
    let cmd = format!(
        "am start -n {MAIN_ACTIVITY} --es proxy_host 127.0.0.1 --ei proxy_port {port} --ez autostart true"
    );
    bridge
        .shell(Some(serial), &cmd)
        .await
        .with_context(|| format!("starting {APP_ID} (is the app installed?)"))?;
    Ok(())
}

/// Removes the reverse. The VPN itself is stopped from the app or its
/// notification; revoking is up to the user.
pub async fn teardown<B: AndroidBridge>(bridge: &B, serial: &str, port: u16) -> Result<()> {
    match bridge
        .adb(Some(serial), &["reverse", "--remove", &tcp(port)])
        .await
    {
        Err(e) if !e.to_string().contains("not found") => Err(e).context("removing adb reverse"),
        _ => Ok(()),
    }
}

pub fn install_steps() -> String {
    format!(
        "Install the CA once on the device (skip if already done):\n  \
         1. Settings > Security (or Security & privacy) > More security settings > Install a certificate > CA certificate\n  \
         2. Pick {DEVICE_CA_PATH} from Downloads\n\
         Then tap Connect in the sieve VPN app and accept Android's VPN prompt. \
         Apps still need to trust user CAs (debug builds: `sieve android nsc`) unless the CA is in the system store (`sieve android inject-ca`)."
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use anyhow::bail;

    use super::*;

    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        fail: Option<&'static str>,
    }

    impl AndroidBridge for Mock {
        async fn adb(&self, serial: Option<&str>, args: &[&str]) -> Result<String> {
            let call = format!("{} {}", serial.unwrap_or("-"), args.join(" "));
            self.calls.lock().unwrap().push(call.clone());
            match self.fail {
                Some(f) if call.contains(f) => bail!("{f} failed"),
                _ => Ok(String::new()),
            }
        }
    }

    #[tokio::test]
    async fn setup_reverses_then_pushes_ca_without_touching_settings() {
        let mock = Mock::default();
        setup(&mock, "dev1", 8730, "PEM").await.unwrap();
        let calls = mock.calls.lock().unwrap().clone();
        assert_eq!(calls[0], "dev1 reverse tcp:8730 tcp:8730");
        assert!(calls[1].starts_with("dev1 push "));
        assert!(calls.iter().all(|c| !c.contains("http_proxy")));
    }

    #[tokio::test]
    async fn launch_passes_the_proxy_port_to_the_app() {
        let mock = Mock::default();
        launch(&mock, "dev1", 9000).await.unwrap();
        let calls = mock.calls.lock().unwrap().clone();
        assert!(calls[0].contains("am start -n dev.sieve.vpn/.MainActivity"));
        assert!(calls[0].contains("--ei proxy_port 9000"));
    }

    #[tokio::test]
    async fn teardown_ignores_a_missing_reverse_but_reports_other_errors() {
        let failing = Mock {
            fail: Some("reverse"),
            ..Default::default()
        };
        assert!(teardown(&failing, "d", 8730).await.is_err());
        struct NotFound;
        impl AndroidBridge for NotFound {
            async fn adb(&self, _: Option<&str>, _: &[&str]) -> Result<String> {
                bail!("listener 'tcp:8730' not found")
            }
        }
        teardown(&NotFound, "d", 8730).await.unwrap();
    }
}
