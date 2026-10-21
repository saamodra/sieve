//! Android mode 1: `adb reverse`, global `http_proxy`, user CA install, NSC
//! snippet, and health check.

use std::{fmt, future::Future, path::PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tracing::warn;

use super::AndroidBridge;

/// Where the CA lands on the device, for Settings > Install certificate.
pub const DEVICE_CA_PATH: &str = "/sdcard/Download/sieve-ca.crt";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// Raw `adb` state: `device`, `unauthorized`, `offline`, ...
    pub state: String,
    pub model: Option<String>,
}

impl Device {
    pub fn is_ready(&self) -> bool {
        self.state == "device"
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.serial)?;
        if let Some(model) = &self.model {
            write!(f, " ({model})")?;
        }
        write!(f, " [{}]", self.state)
    }
}

/// Parses the output of `adb devices -l`.
pub fn parse_devices(output: &str) -> Vec<Device> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let serial = parts.next()?.to_string();
            let state = parts.next()?.to_string();
            let model = parts
                .find_map(|p| p.strip_prefix("model:"))
                .map(|m| m.replace('_', " "));
            Some(Device {
                serial,
                state,
                model,
            })
        })
        .collect()
}

/// Picks the device to work on and returns its serial.
pub async fn select_device<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<String> {
    let devices = parse_devices(&bridge.adb(None, &["devices", "-l"]).await?);
    if let Some(wanted) = serial {
        let device = devices
            .iter()
            .find(|d| d.serial == wanted)
            .ok_or_else(|| anyhow!("device {wanted} not found. {}", describe(&devices)))?;
        return ensure_ready(device);
    }
    match devices.as_slice() {
        [] => bail!(
            "no Android device found. Connect one (or start an emulator) and check `adb devices`"
        ),
        [device] => ensure_ready(device),
        many => bail!(
            "several devices are connected; pass --serial <serial>:\n{}",
            many.iter()
                .map(|d| format!("  {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn describe(devices: &[Device]) -> String {
    if devices.is_empty() {
        return "No devices are connected.".into();
    }
    format!(
        "Connected: {}",
        devices
            .iter()
            .map(Device::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn ensure_ready(device: &Device) -> Result<String> {
    match device.state.as_str() {
        "device" => Ok(device.serial.clone()),
        "unauthorized" => bail!(
            "{device} is unauthorized; accept the USB debugging prompt on the device and retry"
        ),
        "offline" => bail!("{device} is offline; reconnect it or restart adb (`adb kill-server`)"),
        _ => bail!("{device} is not ready"),
    }
}

fn proxy_value(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

fn tcp(port: u16) -> String {
    format!("tcp:{port}")
}

/// Routes the device through the proxy and pushes the CA. If a step fails, the
/// steps already done are undone before the error is returned.
pub async fn setup<B: AndroidBridge>(
    bridge: &B,
    serial: &str,
    port: u16,
    ca_pem: &str,
) -> Result<()> {
    let result = apply(bridge, serial, port, ca_pem).await;
    if result.is_err()
        && let Err(e) = teardown(bridge, serial, port).await
    {
        warn!("cleanup after failed setup also failed: {e:#}");
    }
    result
}

async fn apply<B: AndroidBridge>(bridge: &B, serial: &str, port: u16, ca_pem: &str) -> Result<()> {
    let s = Some(serial);
    let t = tcp(port);
    bridge
        .adb(s, &["reverse", &t, &t])
        .await
        .context("adb reverse")?;
    bridge
        .shell(
            s,
            &format!("settings put global http_proxy {}", proxy_value(port)),
        )
        .await
        .context("setting http_proxy")?;
    push_ca(bridge, serial, ca_pem).await
}

async fn push_ca<B: AndroidBridge>(bridge: &B, serial: &str, ca_pem: &str) -> Result<()> {
    let local: PathBuf =
        std::env::temp_dir().join(format!("sieve-ca-{}.crt", uuid::Uuid::new_v4()));
    tokio::fs::write(&local, ca_pem)
        .await
        .with_context(|| format!("writing {}", local.display()))?;
    let pushed = bridge
        .adb(
            Some(serial),
            &["push", &local.to_string_lossy(), DEVICE_CA_PATH],
        )
        .await
        .context("pushing the CA to the device");
    let _ = tokio::fs::remove_file(&local).await;
    pushed.map(|_| ())
}

/// Restores `http_proxy` and removes the reverse. Safe to run repeatedly.
pub async fn teardown<B: AndroidBridge>(bridge: &B, serial: &str, port: u16) -> Result<()> {
    let s = Some(serial);
    // Attempt both steps even if the first fails.
    let proxy = bridge
        .shell(s, "settings put global http_proxy :0")
        .await
        .context("resetting http_proxy");
    let reverse = match bridge.adb(s, &["reverse", "--remove", &tcp(port)]).await {
        Err(e) if !e.to_string().contains("not found") => Err(e).context("removing adb reverse"),
        _ => Ok(()),
    };
    proxy?;
    reverse
}

/// Awaits `session`, then always runs teardown, whether the session returned
/// `Ok`, `Err`, or the caller shut it down.
pub async fn with_teardown<B, F>(bridge: &B, serial: &str, port: u16, session: F) -> Result<()>
where
    B: AndroidBridge,
    F: Future<Output = Result<()>>,
{
    let result = session.await;
    let cleanup = teardown(bridge, serial, port).await;
    match (result, cleanup) {
        (Err(e), cleanup) => {
            if let Err(c) = cleanup {
                warn!("teardown failed too: {c:#}");
            }
            Err(e)
        }
        (Ok(()), cleanup) => cleanup,
    }
}

pub fn install_steps() -> String {
    format!(
        "Install the CA once on the device (skip if already done):\n  \
         1. Settings > Security (or Security & privacy) > More security settings > Install a certificate > CA certificate\n  \
         2. Pick {DEVICE_CA_PATH} from Downloads\n\
         Apps only trust it with `sieve android nsc` applied (debug builds). Run `sieve android teardown` if sieve exits uncleanly."
    )
}

pub const NSC_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<!-- res/xml/network_security_config.xml -->
<network-security-config>
    <debug-overrides>
        <trust-anchors>
            <certificates src="system" />
            <certificates src="user" />
        </trust-anchors>
    </debug-overrides>
</network-security-config>
"#;

pub const NSC_MANIFEST_LINE: &str =
    r#"android:networkSecurityConfig="@xml/network_security_config""#;

pub fn nsc_snippet() -> String {
    format!(
        "{NSC_XML}\nAdd this attribute to the <application> tag in AndroidManifest.xml:\n\n    {NSC_MANIFEST_LINE}\n\n\
         The debug-overrides block only applies to debuggable builds, so release builds are unaffected.\n"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = match self.status {
            Status::Pass => "pass",
            Status::Fail => "FAIL",
            Status::Skip => "skip",
        };
        write!(f, "[{tag}] {}: {}", self.name, self.detail)
    }
}

pub fn all_passed(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.status != Status::Fail)
}

/// Checks the proxy setting, the reverse, and (if the device has `curl`) that
/// a request from `adb shell` reaches the proxy.
pub async fn health<B: AndroidBridge>(bridge: &B, serial: &str, port: u16) -> Vec<Check> {
    let s = Some(serial);
    let want = proxy_value(port);
    let mut checks = Vec::new();

    checks.push(
        match bridge.shell(s, "settings get global http_proxy").await {
            Ok(out) if out.trim() == want => Check {
                name: "http_proxy",
                status: Status::Pass,
                detail: format!("set to {want}"),
            },
            Ok(out) => Check {
                name: "http_proxy",
                status: Status::Fail,
                detail: format!(
                    "expected {want}, got {:?}; run `sieve android setup`",
                    out.trim()
                ),
            },
            Err(e) => Check {
                name: "http_proxy",
                status: Status::Fail,
                detail: format!("{e:#}"),
            },
        },
    );

    let t = tcp(port);
    checks.push(match bridge.adb(s, &["reverse", "--list"]).await {
        Ok(out)
            if out.lines().any(|l| {
                let mut p = l.split_whitespace().rev();
                p.next() == Some(t.as_str()) && p.next() == Some(t.as_str())
            }) =>
        {
            Check {
                name: "adb reverse",
                status: Status::Pass,
                detail: format!("{t} -> {t} is listed"),
            }
        }
        Ok(_) => Check {
            name: "adb reverse",
            status: Status::Fail,
            detail: format!("{t} is not listed; run `sieve android setup`"),
        },
        Err(e) => Check {
            name: "adb reverse",
            status: Status::Fail,
            detail: format!("{e:#}"),
        },
    });

    let has_curl = bridge
        .shell(s, "command -v curl")
        .await
        .map(|o| !o.trim().is_empty())
        .unwrap_or(false);
    checks.push(if !has_curl {
        Check {
            name: "request via proxy",
            status: Status::Skip,
            detail: "curl is not available on the device".into(),
        }
    } else {
        let cmd = format!(
            "curl -s -m 5 -o /dev/null -w '%{{http_code}}' -x http://{want} http://sieve.health/"
        );
        match bridge.shell(s, &cmd).await {
            Ok(out) if matches!(out.trim().parse::<u16>(), Ok(code) if code != 0) => Check {
                name: "request via proxy",
                status: Status::Pass,
                detail: format!("proxy answered with HTTP {}", out.trim()),
            },
            Ok(_) => Check {
                name: "request via proxy",
                status: Status::Fail,
                detail: "no response from the proxy; is `sieve android setup` running?".into(),
            },
            Err(e) => Check {
                name: "request via proxy",
                status: Status::Fail,
                detail: format!("{e:#}"),
            },
        }
    });
    checks
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Records every call as `[-s serial ]args...`, answers from `replies`
    /// (first entry whose key is a prefix of the call), and fails calls
    /// matching a `fail` prefix.
    #[derive(Default)]
    struct Mock {
        calls: Mutex<Vec<String>>,
        replies: Vec<(&'static str, &'static str)>,
        fail: Vec<(&'static str, &'static str)>,
    }

    impl Mock {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AndroidBridge for Mock {
        async fn adb(&self, serial: Option<&str>, args: &[&str]) -> Result<String> {
            let mut call = String::new();
            if let Some(s) = serial {
                call.push_str(&format!("-s {s} "));
            }
            call.push_str(&args.join(" "));
            self.calls.lock().unwrap().push(call.clone());
            if let Some((_, msg)) = self.fail.iter().find(|(k, _)| call.contains(k)) {
                bail!("{msg}");
            }
            Ok(self
                .replies
                .iter()
                .find(|(k, _)| call.contains(k))
                .map(|(_, v)| v.to_string())
                .unwrap_or_default())
        }
    }

    const ONE: &str = "List of devices attached\nemulator-5554          device product:sdk_gphone64 model:sdk_gphone64_arm64 device:emu64a transport_id:1\n";

    #[test]
    fn parses_device_states_and_models() {
        let out = "* daemon started successfully\nList of devices attached\n\
            R58M123 device usb:1-1 product:x model:Pixel_7 device:panther transport_id:2\n\
            ABC123 unauthorized usb:1-2 transport_id:3\n\
            XYZ789 offline\n\n";
        let d = parse_devices(out);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].serial, "R58M123");
        assert!(d[0].is_ready());
        assert_eq!(d[0].model.as_deref(), Some("Pixel 7"));
        assert_eq!(d[1].state, "unauthorized");
        assert_eq!(d[1].model, None);
        assert_eq!(d[2].state, "offline");
        assert!(parse_devices("List of devices attached\n\n").is_empty());
    }

    #[tokio::test]
    async fn select_zero_devices_errors() {
        let m = Mock {
            replies: vec![("devices", "List of devices attached\n\n")],
            ..Default::default()
        };
        let e = select_device(&m, None).await.unwrap_err().to_string();
        assert!(e.contains("no Android device"), "{e}");
    }

    #[tokio::test]
    async fn select_single_device() {
        let m = Mock {
            replies: vec![("devices", ONE)],
            ..Default::default()
        };
        assert_eq!(select_device(&m, None).await.unwrap(), "emulator-5554");
        assert_eq!(m.calls(), ["devices -l"]);
    }

    #[tokio::test]
    async fn select_many_requires_serial_and_lists_them() {
        let two = "List of devices attached\nA1 device model:Pixel_7\nB2 device model:Galaxy_S9\n";
        let m = Mock {
            replies: vec![("devices", two)],
            ..Default::default()
        };
        let e = select_device(&m, None).await.unwrap_err().to_string();
        assert!(
            e.contains("--serial") && e.contains("A1") && e.contains("B2"),
            "{e}"
        );
        assert!(e.contains("Galaxy S9"), "{e}");
        assert_eq!(select_device(&m, Some("B2")).await.unwrap(), "B2");
        let e = select_device(&m, Some("C3")).await.unwrap_err().to_string();
        assert!(e.contains("not found") && e.contains("A1"), "{e}");
    }

    #[tokio::test]
    async fn select_rejects_unauthorized_and_offline() {
        for (state, hint) in [("unauthorized", "USB debugging"), ("offline", "offline")] {
            let out: &'static str =
                Box::leak(format!("List of devices attached\nD1 {state}\n").into_boxed_str());
            let m = Mock {
                replies: vec![("devices", out)],
                ..Default::default()
            };
            let e = select_device(&m, None).await.unwrap_err().to_string();
            assert!(e.contains(state) && e.contains(hint), "{e}");
        }
    }

    #[tokio::test]
    async fn setup_runs_exact_adb_sequence_with_config_port() {
        let m = Mock::default();
        setup(&m, "emu", 9090, "PEM").await.unwrap();
        let calls = m.calls();
        assert_eq!(calls.len(), 3, "{calls:?}");
        assert_eq!(calls[0], "-s emu reverse tcp:9090 tcp:9090");
        assert_eq!(
            calls[1],
            "-s emu shell settings put global http_proxy 127.0.0.1:9090"
        );
        assert!(calls[2].starts_with("-s emu push "), "{}", calls[2]);
        assert!(
            calls[2].ends_with(" /sdcard/Download/sieve-ca.crt"),
            "{}",
            calls[2]
        );
    }

    #[tokio::test]
    async fn failed_setup_cleans_up() {
        let m = Mock {
            fail: vec![("push", "no space")],
            ..Default::default()
        };
        let e = setup(&m, "emu", 8080, "PEM").await.unwrap_err();
        assert!(format!("{e:#}").contains("no space"));
        let calls = m.calls();
        assert_eq!(
            calls[calls.len() - 2..],
            [
                "-s emu shell settings put global http_proxy :0",
                "-s emu reverse --remove tcp:8080"
            ]
        );
    }

    #[tokio::test]
    async fn teardown_sequence_and_idempotent() {
        let m = Mock {
            fail: vec![(
                "reverse --remove",
                "adb: error: listener 'tcp:8080' not found",
            )],
            ..Default::default()
        };
        teardown(&m, "emu", 8080).await.unwrap();
        teardown(&m, "emu", 8080).await.unwrap();
        assert_eq!(
            m.calls()[..2],
            [
                "-s emu shell settings put global http_proxy :0",
                "-s emu reverse --remove tcp:8080"
            ]
        );
    }

    #[tokio::test]
    async fn teardown_reports_real_reverse_errors() {
        let m = Mock {
            fail: vec![("reverse --remove", "device offline")],
            ..Default::default()
        };
        assert!(teardown(&m, "emu", 8080).await.is_err());
        // The proxy reset still ran.
        assert_eq!(m.calls().len(), 2);
    }

    #[tokio::test]
    async fn teardown_runs_after_failing_or_ok_session() {
        for session in [Err(anyhow!("proxy crashed")), Ok(())] {
            let failed = session.is_err();
            let m = Mock::default();
            let r = with_teardown(&m, "emu", 8080, async { session }).await;
            assert_eq!(r.is_err(), failed);
            assert_eq!(
                m.calls(),
                [
                    "-s emu shell settings put global http_proxy :0",
                    "-s emu reverse --remove tcp:8080"
                ]
            );
        }
    }

    #[test]
    fn nsc_has_debug_overrides_and_user_certs() {
        let s = nsc_snippet();
        assert!(s.contains("<debug-overrides>"));
        assert!(s.contains(r#"<certificates src="user""#));
        assert!(s.contains("android:networkSecurityConfig"));
    }

    #[tokio::test]
    async fn health_all_pass() {
        let m = Mock {
            replies: vec![
                ("settings get", "127.0.0.1:8080\n"),
                ("reverse --list", "emu tcp:8080 tcp:8080\n"),
                ("command -v", "/system/bin/curl\n"),
                ("curl -s", "502"),
            ],
            ..Default::default()
        };
        let checks = health(&m, "emu", 8080).await;
        assert_eq!(checks.len(), 3);
        assert!(
            checks.iter().all(|c| c.status == Status::Pass),
            "{checks:?}"
        );
        assert!(all_passed(&checks));
    }

    #[tokio::test]
    async fn health_reports_failures_and_skips_without_curl() {
        let m = Mock {
            replies: vec![("settings get", ":0\n"), ("reverse --list", "")],
            ..Default::default()
        };
        let checks = health(&m, "emu", 8080).await;
        assert_eq!(checks[0].status, Status::Fail);
        assert_eq!(checks[1].status, Status::Fail);
        assert_eq!(checks[2].status, Status::Skip);
        assert!(!all_passed(&checks));
    }

    #[tokio::test]
    async fn health_curl_without_response_fails() {
        let m = Mock {
            replies: vec![
                ("settings get", "127.0.0.1:8080"),
                ("reverse --list", "emu tcp:8080 tcp:8080"),
                ("command -v", "/system/bin/curl"),
                ("curl -s", "000"),
            ],
            ..Default::default()
        };
        assert_eq!(health(&m, "emu", 8080).await[2].status, Status::Fail);
    }
}
