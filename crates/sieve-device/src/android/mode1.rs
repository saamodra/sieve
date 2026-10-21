//! Android mode 1: `adb reverse`, global `http_proxy`, user CA install, NSC
//! snippet, and health check.

use std::{
    fmt, fs,
    future::Future,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

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

/// What the device's `http_proxy` was before sieve touched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProxy {
    pub serial: String,
    /// `None` when the device had no proxy.
    pub http_proxy: Option<String>,
}

#[derive(Debug)]
pub struct LeftoverProxy {
    pub path: PathBuf,
    pub state: SavedProxy,
}

const STATE_PREFIX: &str = "android-proxy-";

/// One file per device, so two devices can be set up at the same time.
pub fn state_file(state_dir: &Path, serial: &str) -> PathBuf {
    let safe: String = serial
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    state_dir.join(format!("{STATE_PREFIX}{safe}.json"))
}

/// `settings get` prints `null` when unset, and `:0` also means no proxy.
fn parse_http_proxy(output: &str) -> Option<String> {
    match output.trim() {
        "" | "null" | ":0" => None,
        value => Some(value.to_string()),
    }
}

/// True for a proxy on the device's own loopback, which is what `setup` sets
/// (through `adb reverse`) and is dead once sieve is gone. The port may differ
/// between runs.
fn is_loopback_proxy(value: &str) -> bool {
    let host = value.rsplit_once(':').map_or(value, |(host, _)| host);
    matches!(
        host.trim_matches(['[', ']']),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Saves the device's current `http_proxy` before sieve changes it. An
/// existing file is kept: it holds the original value, while the device may
/// by now show sieve's own proxy.
async fn save_proxy<B: AndroidBridge>(bridge: &B, state_dir: &Path, serial: &str) -> Result<()> {
    let file = state_file(state_dir, serial);
    if file.exists() {
        return Ok(());
    }
    let current = bridge
        .shell(Some(serial), "settings get global http_proxy")
        .await
        .context("reading http_proxy")?;
    let mut http_proxy = parse_http_proxy(&current);
    if let Some(stale) = http_proxy.take_if(|v| is_loopback_proxy(v)) {
        warn!(
            serial,
            "found a stale sieve proxy ({stale}) on the device; treating it as no previous proxy"
        );
    }
    let state = SavedProxy {
        serial: serial.to_string(),
        http_proxy,
    };
    fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    fs::write(&file, serde_json::to_vec_pretty(&state)?)
        .with_context(|| format!("writing {}", file.display()))
}

fn load_state(file: &Path) -> Result<SavedProxy> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))
}

/// The saved states that runs which never tore down left behind.
pub fn find_leftovers(state_dir: &Path) -> Result<Vec<LeftoverProxy>> {
    let Ok(entries) = fs::read_dir(state_dir) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for entry in entries {
        let path = entry?.path();
        let is_state = path.extension().is_some_and(|e| e == "json")
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(STATE_PREFIX));
        if is_state {
            let state = load_state(&path)?;
            found.push(LeftoverProxy { path, state });
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

/// Puts the saved `http_proxy` back (`:0` if there was none) and deletes the
/// state file. The file is kept if the device rejects the change, so it can be
/// retried. Without a state file this resets the proxy to `:0`.
pub async fn restore_proxy<B: AndroidBridge>(
    bridge: &B,
    state_dir: &Path,
    serial: &str,
) -> Result<()> {
    let file = state_file(state_dir, serial);
    let value = if file.exists() {
        load_state(&file)?.http_proxy.unwrap_or_else(|| ":0".into())
    } else {
        ":0".into()
    };
    bridge
        .shell(
            Some(serial),
            &format!("settings put global http_proxy {value}"),
        )
        .await
        .context("restoring http_proxy")?;
    if file.exists() {
        fs::remove_file(&file).with_context(|| format!("removing {}", file.display()))?;
    }
    info!(serial, "device http_proxy restored");
    Ok(())
}

/// Routes the device through the proxy and pushes the CA. If a step fails, the
/// steps already done are undone before the error is returned.
pub async fn setup<B: AndroidBridge>(
    bridge: &B,
    state_dir: &Path,
    serial: &str,
    port: u16,
    ca_pem: &str,
) -> Result<()> {
    // Nothing has changed yet, so a failure here needs no cleanup.
    save_proxy(bridge, state_dir, serial).await?;
    let result = apply(bridge, serial, port, ca_pem).await;
    if result.is_err()
        && let Err(e) = teardown(bridge, state_dir, serial, port).await
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
pub async fn teardown<B: AndroidBridge>(
    bridge: &B,
    state_dir: &Path,
    serial: &str,
    port: u16,
) -> Result<()> {
    let s = Some(serial);
    // Attempt both steps even if the first fails.
    let proxy = restore_proxy(bridge, state_dir, serial).await;
    let reverse = match bridge.adb(s, &["reverse", "--remove", &tcp(port)]).await {
        Err(e) if !e.to_string().contains("not found") => Err(e).context("removing adb reverse"),
        _ => Ok(()),
    };
    proxy?;
    reverse
}

/// Awaits `session`, then always runs teardown, whether the session returned
/// `Ok`, `Err`, or the caller shut it down.
pub async fn with_teardown<B, F>(
    bridge: &B,
    state_dir: &Path,
    serial: &str,
    port: u16,
    session: F,
) -> Result<()>
where
    B: AndroidBridge,
    F: Future<Output = Result<()>>,
{
    let result = session.await;
    let cleanup = teardown(bridge, state_dir, serial, port).await;
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
        let dir = tempfile::tempdir().unwrap();
        setup(&m, dir.path(), "emu", 9090, "PEM").await.unwrap();
        let calls = m.calls();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert_eq!(calls[0], "-s emu shell settings get global http_proxy");
        assert_eq!(calls[1], "-s emu reverse tcp:9090 tcp:9090");
        assert_eq!(
            calls[2],
            "-s emu shell settings put global http_proxy 127.0.0.1:9090"
        );
        assert!(calls[3].starts_with("-s emu push "), "{}", calls[3]);
        assert!(
            calls[3].ends_with(" /sdcard/Download/sieve-ca.crt"),
            "{}",
            calls[3]
        );
    }

    #[tokio::test]
    async fn failed_setup_cleans_up() {
        let m = Mock {
            fail: vec![("push", "no space")],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let e = setup(&m, dir.path(), "emu", 8080, "PEM").await.unwrap_err();
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
        let dir = tempfile::tempdir().unwrap();
        teardown(&m, dir.path(), "emu", 8080).await.unwrap();
        teardown(&m, dir.path(), "emu", 8080).await.unwrap();
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
        let dir = tempfile::tempdir().unwrap();
        assert!(teardown(&m, dir.path(), "emu", 8080).await.is_err());
        // The proxy reset still ran.
        assert_eq!(m.calls().len(), 2);
    }

    #[tokio::test]
    async fn teardown_runs_after_failing_or_ok_session() {
        for session in [Err(anyhow!("proxy crashed")), Ok(())] {
            let failed = session.is_err();
            let m = Mock::default();
            let dir = tempfile::tempdir().unwrap();
            let r = with_teardown(&m, dir.path(), "emu", 8080, async { session }).await;
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

    #[tokio::test]
    async fn teardown_restores_previous_proxy_and_removes_state() {
        let m = Mock {
            replies: vec![("settings get", "corp.example:3128\n")],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        setup(&m, dir.path(), "emu", 8080, "PEM").await.unwrap();
        assert!(state_file(dir.path(), "emu").exists());
        teardown(&m, dir.path(), "emu", 8080).await.unwrap();
        let calls = m.calls();
        assert_eq!(
            calls[calls.len() - 2..],
            [
                "-s emu shell settings put global http_proxy corp.example:3128",
                "-s emu reverse --remove tcp:8080"
            ]
        );
        assert!(!state_file(dir.path(), "emu").exists());
    }

    #[tokio::test]
    async fn teardown_without_previous_proxy_resets_to_zero() {
        for unset in ["null\n", ":0\n", ""] {
            let m = Mock {
                replies: vec![("settings get", unset)],
                ..Default::default()
            };
            let dir = tempfile::tempdir().unwrap();
            setup(&m, dir.path(), "emu", 8080, "PEM").await.unwrap();
            assert_eq!(
                find_leftovers(dir.path()).unwrap()[0].state.http_proxy,
                None
            );
            teardown(&m, dir.path(), "emu", 8080).await.unwrap();
            assert!(
                m.calls()
                    .contains(&"-s emu shell settings put global http_proxy :0".to_string())
            );
            assert!(find_leftovers(dir.path()).unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn second_setup_keeps_the_original_saved_proxy() {
        let dir = tempfile::tempdir().unwrap();
        let first = Mock {
            replies: vec![("settings get", "corp.example:3128")],
            ..Default::default()
        };
        setup(&first, dir.path(), "emu", 8080, "PEM").await.unwrap();
        // The device now shows sieve's own proxy.
        let second = Mock {
            replies: vec![("settings get", "127.0.0.1:8080")],
            ..Default::default()
        };
        setup(&second, dir.path(), "emu", 8080, "PEM")
            .await
            .unwrap();
        assert!(!second.calls().iter().any(|c| c.contains("settings get")));
        let left = find_leftovers(dir.path()).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].state.serial, "emu");
        assert_eq!(
            left[0].state.http_proxy.as_deref(),
            Some("corp.example:3128")
        );
    }

    #[tokio::test]
    async fn state_is_kept_when_restore_fails() {
        let m = Mock {
            replies: vec![("settings get", "corp.example:3128")],
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        setup(&m, dir.path(), "emu", 8080, "PEM").await.unwrap();
        let broken = Mock {
            fail: vec![("settings put", "device offline")],
            ..Default::default()
        };
        assert!(restore_proxy(&broken, dir.path(), "emu").await.is_err());
        assert_eq!(find_leftovers(dir.path()).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stale_sieve_proxy_is_not_saved_as_previous() {
        for stale in ["127.0.0.1:9999", "localhost:8080", "::1:8080", "[::1]:8080"] {
            let m = Mock {
                replies: vec![("settings get", stale)],
                ..Default::default()
            };
            let dir = tempfile::tempdir().unwrap();
            setup(&m, dir.path(), "emu", 8080, "PEM").await.unwrap();
            assert_eq!(
                find_leftovers(dir.path()).unwrap()[0].state.http_proxy,
                None,
                "{stale}"
            );
            teardown(&m, dir.path(), "emu", 8080).await.unwrap();
            assert!(
                m.calls()
                    .contains(&"-s emu shell settings put global http_proxy :0".to_string())
            );
        }
    }

    #[test]
    fn state_file_name_is_safe_for_network_serials() {
        let f = state_file(Path::new("/s"), "192.168.1.5:5555");
        assert_eq!(f, Path::new("/s/android-proxy-192.168.1.5_5555.json"));
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
