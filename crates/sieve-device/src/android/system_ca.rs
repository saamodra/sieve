//! Android mode 2: inject the sieve CA into the system trust store on
//! emulators and rooted devices.
//!
//! The work happens on the device in `scripts/android/inject-ca.sh`, which is
//! embedded here, pushed next to the cert, and run as root.

use std::{fs, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use super::AndroidBridge;

const SCRIPT: &str = include_str!("../../../../scripts/android/inject-ca.sh");
const REMOTE_DIR: &str = "/data/local/tmp";
const REMOTE_SCRIPT: &str = "/data/local/tmp/sieve-inject-ca.sh";
/// The script's last line on success.
const SUCCESS_MARKER: &str = "sieve-inject-ca: OK:";

/// How long to wait for adbd to come back after `adb root` restarts it.
const ROOT_RETRIES: u32 = 10;
const ROOT_RETRY_DELAY: Duration = Duration::from_millis(500);

const NO_ROOT: &str = "system CA injection needs root, but neither `adb root` nor `su` works \
on this device. Emulator images labelled \"Google Play\" can't `adb root`; use one labelled \
\"Google APIs\" instead, or a rooted device with `su`. Without root, use `sieve android setup` \
(user CA, mode 1).";

/// How the script gets root on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootMethod {
    /// adbd itself runs as root (`adb root`), so `adb shell` is root.
    AdbRoot,
    /// adbd is unprivileged, but `su -c` works.
    Su,
}

/// Injects `ca_pem` as `<subject_hash_old>.0` into the device's system CA
/// store, for both the legacy `/system` store and the Android 14+ APEX store.
/// The change does not survive a reboot.
pub async fn inject<B: AndroidBridge>(
    bridge: &B,
    serial: Option<&str>,
    ca_pem: &str,
    subject_hash_old: &str,
) -> Result<()> {
    let hash = subject_hash_old.trim();
    // It ends up in a device shell command line, so keep it to what openssl emits.
    if hash.len() != 8 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("invalid subject_hash_old {hash:?}: expected 8 hex digits");
    }
    let hash = hash.to_ascii_lowercase();

    let root = acquire_root(bridge, serial).await?;

    let cert_name = format!("{hash}.0");
    let remote_cert = format!("{REMOTE_DIR}/{cert_name}");
    let local = LocalDir::new()?;
    push(
        bridge,
        serial,
        &local.write(&cert_name, ca_pem)?,
        &remote_cert,
    )
    .await?;
    push(
        bridge,
        serial,
        &local.write("inject-ca.sh", SCRIPT)?,
        REMOTE_SCRIPT,
    )
    .await?;

    let run = format!("sh {REMOTE_SCRIPT} {remote_cert}");
    let command = match root {
        RootMethod::AdbRoot => run,
        RootMethod::Su => format!("su -c '{run}'"),
    };
    let result = bridge.shell(serial, &command).await;
    // Best effort: the files are harmless if they stay behind.
    let _ = bridge
        .shell(serial, &format!("rm -f {REMOTE_SCRIPT} {remote_cert}"))
        .await;

    let output = result.context("inject-ca.sh failed on the device")?;
    if !output.contains(SUCCESS_MARKER) {
        bail!("inject-ca.sh did not report success:\n{}", output.trim());
    }
    for line in output.lines() {
        println!("{line}");
    }
    Ok(())
}

/// Prefers `adb root` (Google APIs emulator images), then `su -c` (rooted
/// devices). Fails with [`NO_ROOT`] if neither gives uid 0.
async fn acquire_root<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<RootMethod> {
    // Also surfaces "no device" errors before we blame root.
    if is_root(&bridge.shell(serial, "id -u").await?) {
        return Ok(RootMethod::AdbRoot);
    }

    // Production builds refuse, by exit code or (older adb) only by message.
    if let Ok(out) = bridge.adb(serial, &["root"]).await
        && !out.contains("cannot run as root")
    {
        // adbd restarts as root, so the device drops off for a moment.
        let _ = bridge.adb(serial, &["wait-for-device"]).await;
        for attempt in 1..=ROOT_RETRIES {
            if let Ok(out) = bridge.shell(serial, "id -u").await
                && is_root(&out)
            {
                return Ok(RootMethod::AdbRoot);
            }
            if attempt < ROOT_RETRIES {
                tokio::time::sleep(ROOT_RETRY_DELAY).await;
            }
        }
    }

    if let Ok(out) = bridge.shell(serial, "su -c 'id -u'").await
        && is_root(&out)
    {
        return Ok(RootMethod::Su);
    }

    bail!(NO_ROOT)
}

fn is_root(id_output: &str) -> bool {
    id_output.trim() == "0"
}

async fn push<B: AndroidBridge>(
    bridge: &B,
    serial: Option<&str>,
    local: &std::path::Path,
    remote: &str,
) -> Result<()> {
    let local_str = local
        .to_str()
        .with_context(|| format!("temp path {} is not UTF-8", local.display()))?;
    bridge
        .adb(serial, &["push", local_str, remote])
        .await
        .with_context(|| format!("pushing {remote}"))?;
    Ok(())
}

/// A host temp dir for the files to push, removed on drop.
struct LocalDir(PathBuf);

impl LocalDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!("sieve-inject-ca-{}", Uuid::new_v4()));
        fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?;
        Ok(Self(path))
    }

    fn write(&self, name: &str, contents: &str) -> Result<PathBuf> {
        let path = self.0.join(name);
        fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}

impl Drop for LocalDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicBool, Ordering},
        },
    };

    use super::*;

    const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    const HASH: &str = "9a5ba575";
    const RUN: &str = "sh /data/local/tmp/sieve-inject-ca.sh /data/local/tmp/9a5ba575.0";
    const OK_OUTPUT: &str = "sieve-inject-ca: Android API level 34\n\
        sieve-inject-ca: OK: 9a5ba575.0 is in /system/etc/security/cacerts (API 34)\n";

    struct Push {
        local_name: String,
        contents: String,
        remote: String,
    }

    /// Fakes `adb` for a device whose root options are set by the flags.
    struct MockBridge {
        already_root: bool,
        adb_root_works: bool,
        su_works: bool,
        script_result: fn() -> Result<String>,
        rooted: AtomicBool,
        calls: Mutex<Vec<Vec<String>>>,
        pushes: Mutex<Vec<Push>>,
    }

    impl MockBridge {
        fn new() -> Self {
            Self {
                already_root: false,
                adb_root_works: false,
                su_works: false,
                script_result: || Ok(OK_OUTPUT.to_owned()),
                rooted: AtomicBool::new(false),
                calls: Mutex::new(Vec::new()),
                pushes: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }

        fn shell_commands(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|c| c[0] == "shell")
                .map(|c| c[1].clone())
                .collect()
        }
    }

    impl AndroidBridge for MockBridge {
        async fn adb(&self, serial: Option<&str>, args: &[&str]) -> Result<String> {
            assert_eq!(serial, Some("emulator-5554"));
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|a| a.to_string()).collect());
            let root_now = self.already_root || self.rooted.load(Ordering::SeqCst);
            match args {
                ["shell", "id -u"] => Ok(if root_now { "0\n" } else { "2000\n" }.into()),
                ["root"] if self.adb_root_works => {
                    self.rooted.store(true, Ordering::SeqCst);
                    Ok("restarting adbd as root\n".into())
                }
                ["root"] => Ok("adbd cannot run as root in production builds\n".into()),
                ["wait-for-device"] => Ok(String::new()),
                ["shell", "su -c 'id -u'"] if self.su_works => Ok("0\n".into()),
                ["shell", "su -c 'id -u'"] => {
                    bail!("/system/bin/sh: su: inaccessible or not found")
                }
                ["push", local, remote] => {
                    let local = Path::new(local);
                    self.pushes.lock().unwrap().push(Push {
                        local_name: local.file_name().unwrap().to_string_lossy().into(),
                        contents: fs::read_to_string(local).unwrap(),
                        remote: remote.to_string(),
                    });
                    Ok("1 file pushed\n".into())
                }
                ["shell", cmd] if cmd.starts_with("rm -f ") => Ok(String::new()),
                ["shell", cmd] if cmd.contains("sieve-inject-ca.sh") => (self.script_result)(),
                _ => panic!("unexpected adb call: {args:?}"),
            }
        }
    }

    async fn run(bridge: &MockBridge) -> Result<()> {
        inject(bridge, Some("emulator-5554"), PEM, HASH).await
    }

    #[tokio::test]
    async fn refuses_without_root_and_names_the_right_images() {
        let bridge = MockBridge::new();
        let err = run(&bridge).await.unwrap_err().to_string();
        assert!(err.contains("\"Google Play\""), "{err}");
        assert!(err.contains("\"Google APIs\""), "{err}");
        assert!(bridge.pushes.lock().unwrap().is_empty());
        assert!(bridge.calls().contains(&vec!["root".to_string()]));
    }

    #[tokio::test]
    async fn uses_adb_root_when_available() {
        let bridge = MockBridge {
            adb_root_works: true,
            su_works: true,
            ..MockBridge::new()
        };
        run(&bridge).await.unwrap();
        let calls = bridge.calls();
        assert!(calls.contains(&vec!["root".to_string()]));
        assert!(calls.contains(&vec!["wait-for-device".to_string()]));
        let shells = bridge.shell_commands();
        assert!(shells.contains(&RUN.to_string()), "{shells:?}");
        assert!(!shells.iter().any(|c| c.starts_with("su ")), "{shells:?}");
    }

    #[tokio::test]
    async fn skips_adb_root_when_adbd_is_already_root() {
        let bridge = MockBridge {
            already_root: true,
            ..MockBridge::new()
        };
        run(&bridge).await.unwrap();
        assert!(!bridge.calls().contains(&vec!["root".to_string()]));
        assert!(bridge.shell_commands().contains(&RUN.to_string()));
    }

    #[tokio::test]
    async fn falls_back_to_su() {
        let bridge = MockBridge {
            su_works: true,
            ..MockBridge::new()
        };
        run(&bridge).await.unwrap();
        assert!(bridge.shell_commands().contains(&format!("su -c '{RUN}'")));
    }

    #[tokio::test]
    async fn pushes_the_cert_as_hash_dot_0_and_the_script() {
        let bridge = MockBridge {
            adb_root_works: true,
            ..MockBridge::new()
        };
        run(&bridge).await.unwrap();
        let pushes = bridge.pushes.lock().unwrap();
        assert_eq!(pushes.len(), 2);
        assert_eq!(pushes[0].local_name, "9a5ba575.0");
        assert_eq!(pushes[0].remote, "/data/local/tmp/9a5ba575.0");
        assert_eq!(pushes[0].contents, PEM);
        assert_eq!(pushes[1].remote, REMOTE_SCRIPT);
        assert_eq!(pushes[1].contents, SCRIPT);
        // Pushed files are cleaned up on the device afterwards.
        assert_eq!(
            bridge.shell_commands().last().unwrap(),
            "rm -f /data/local/tmp/sieve-inject-ca.sh /data/local/tmp/9a5ba575.0"
        );
    }

    #[tokio::test]
    async fn script_failure_is_an_error() {
        let bridge = MockBridge {
            adb_root_works: true,
            script_result: || {
                bail!("adb shell failed (exit status: 1): sieve-inject-ca: FAILED: no zygote")
            },
            ..MockBridge::new()
        };
        let err = format!("{:#}", run(&bridge).await.unwrap_err());
        assert!(err.contains("inject-ca.sh failed"), "{err}");
        assert!(err.contains("FAILED: no zygote"), "{err}");
        assert!(
            bridge
                .shell_commands()
                .last()
                .unwrap()
                .starts_with("rm -f ")
        );
    }

    #[tokio::test]
    async fn output_without_success_line_is_an_error() {
        // Old adb without the shell protocol exits 0 whatever the script does.
        let bridge = MockBridge {
            adb_root_works: true,
            script_result: || Ok("sieve-inject-ca: FAILED: no zygote process found\n".into()),
            ..MockBridge::new()
        };
        let err = run(&bridge).await.unwrap_err().to_string();
        assert!(err.contains("did not report success"), "{err}");
    }

    #[tokio::test]
    async fn rejects_a_malformed_hash_before_touching_the_device() {
        let bridge = MockBridge::new();
        let err = inject(&bridge, Some("emulator-5554"), PEM, "abc; reboot")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("subject_hash_old"), "{err}");
        assert!(bridge.calls().is_empty());
    }

    #[test]
    fn script_prints_the_marker_rust_looks_for() {
        assert!(SCRIPT.contains("TAG=sieve-inject-ca\n"));
        assert!(SCRIPT.contains(r#"log "OK: "#));
        assert_eq!(SUCCESS_MARKER, "sieve-inject-ca: OK:");
    }
}
