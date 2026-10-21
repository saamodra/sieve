//! Android `frida` workflow: push `frida-server` for the device's ABI, start
//! it, and spawn an app with sieve's certificate-pinning unpinning script.
//!
//! `frida-server` itself is not bundled (it is tens of MB per ABI, and tied
//! to the exact frida version on the host running `frida`), so the caller
//! supplies a path to one already downloaded from
//! <https://github.com/frida/frida/releases>. The unpinning script is
//! bundled: see `scripts/frida/android-unpinning.js` and `docs/frida.md` for
//! the license note on why it's an original script rather than a vendored
//! one.

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::process::Command;
use uuid::Uuid;

use super::{AndroidBridge, system_ca};

/// Bundled, sieve-authored certificate-pinning bypass script.
pub const UNPINNING_SCRIPT: &str = include_str!("../../../../scripts/frida/android-unpinning.js");

const REMOTE_SERVER: &str = "/data/local/tmp/sieve-frida-server";
/// How long to give `frida-server` to come up and start listening.
const START_RETRIES: u32 = 10;
const START_RETRY_DELAY: Duration = Duration::from_millis(300);

const NO_ROOT: &str = "frida-server needs root, but neither `adb root` nor `su` works on this device. \
Emulator images labelled \"Google Play\" can't `adb root`; use one labelled \"Google APIs\" \
instead, or a rooted device with `su`.";

/// The device ABIs sieve knows how to map to a `frida-server` release asset
/// and an ELF `e_machine` value, to catch a mismatched binary before pushing
/// tens of MB to the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abi {
    Arm64,
    Arm,
    X86_64,
    X86,
}

impl Abi {
    /// Parses `ro.product.cpu.abi` (e.g. `arm64-v8a`).
    pub fn parse(prop: &str) -> Result<Self> {
        match prop.trim() {
            "arm64-v8a" => Ok(Abi::Arm64),
            "armeabi-v7a" | "armeabi" => Ok(Abi::Arm),
            "x86_64" => Ok(Abi::X86_64),
            "x86" => Ok(Abi::X86),
            other => bail!("unrecognized device ABI {other:?}"),
        }
    }

    /// The architecture name used in `frida-server-<version>-android-<arch>`
    /// release asset names.
    pub fn frida_asset_arch(self) -> &'static str {
        match self {
            Abi::Arm64 => "arm64",
            Abi::Arm => "arm",
            Abi::X86_64 => "x86_64",
            Abi::X86 => "x86",
        }
    }

    /// The ELF `e_machine` value a `frida-server` binary for this ABI must
    /// have.
    fn elf_machine(self) -> u16 {
        match self {
            Abi::Arm64 => 183, // EM_AARCH64
            Abi::Arm => 40,    // EM_ARM
            Abi::X86_64 => 62, // EM_X86_64
            Abi::X86 => 3,     // EM_386
        }
    }
}

/// Reads the device's `ro.product.cpu.abi` and parses it.
pub async fn device_abi<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<Abi> {
    let prop = bridge
        .shell(serial, "getprop ro.product.cpu.abi")
        .await
        .context("reading ro.product.cpu.abi")?;
    Abi::parse(&prop)
}

/// Checks that `binary`'s ELF `e_machine` field matches `abi`, to catch the
/// wrong `frida-server` download before spending time pushing it.
pub fn check_binary_abi(binary: &Path, abi: Abi) -> Result<()> {
    let bytes = fs::read(binary).with_context(|| format!("reading {}", binary.display()))?;
    if bytes.len() < 20 || &bytes[0..4] != b"\x7fELF" {
        bail!("{} does not look like an ELF binary", binary.display());
    }
    let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
    let want = abi.elf_machine();
    if machine != want {
        bail!(
            "{} is built for ELF machine {machine}, but the device is {:?} (wants {want}); \
             download the matching frida-server-*-android-{} release",
            binary.display(),
            abi,
            abi.frida_asset_arch()
        );
    }
    Ok(())
}

/// Pushes `local_binary` to the device and makes it executable.
pub async fn push_server<B: AndroidBridge>(
    bridge: &B,
    serial: Option<&str>,
    local_binary: &Path,
) -> Result<()> {
    let local_str = local_binary
        .to_str()
        .with_context(|| format!("path {} is not UTF-8", local_binary.display()))?;
    bridge
        .adb(serial, &["push", local_str, REMOTE_SERVER])
        .await
        .context("pushing frida-server")?;
    bridge
        .shell(serial, &format!("chmod 755 {REMOTE_SERVER}"))
        .await
        .context("chmod frida-server")?;
    Ok(())
}

/// True if a process named `sieve-frida-server` is running on the device.
pub async fn is_server_running<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<bool> {
    let out = bridge
        .shell(serial, "ps -A 2>/dev/null | grep -c sieve-frida-server")
        .await
        .unwrap_or_default();
    Ok(out.trim().parse::<u32>().unwrap_or(0) > 0)
}

/// Starts the previously-pushed `frida-server` as root, backgrounded so the
/// `adb shell` call returns. Waits until it shows up in the process list.
pub async fn start_server<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<()> {
    let root = system_ca::acquire_root(bridge, serial)
        .await
        .map_err(|_| anyhow::anyhow!(NO_ROOT))?;
    let run = format!("nohup {REMOTE_SERVER} >/dev/null 2>&1 &");
    bridge
        .shell(serial, &root.wrap(&run))
        .await
        .context("starting frida-server")?;

    for attempt in 1..=START_RETRIES {
        if is_server_running(bridge, serial).await? {
            return Ok(());
        }
        if attempt < START_RETRIES {
            tokio::time::sleep(START_RETRY_DELAY).await;
        }
    }
    bail!("frida-server did not show up in the process list after starting it")
}

/// Stops the device-side `frida-server`. Best effort: errors are ignored by
/// callers that run this during teardown.
pub async fn stop_server<B: AndroidBridge>(bridge: &B, serial: Option<&str>) -> Result<()> {
    bridge
        .shell(serial, "pkill -f sieve-frida-server")
        .await
        .map(|_| ())
}

/// Runs an app on the host side with a Frida script attached, via the
/// `frida` CLI (from `frida-tools`, installed separately on the host).
/// Mocked in tests behind this trait.
pub trait FridaHost: Send + Sync {
    /// Spawns `package` (suspended), attaches `script`, and lets it run.
    /// Returns once the app has been spawned and the script loaded; the app
    /// itself keeps running until the device session ends.
    fn spawn(
        &self,
        serial: Option<&str>,
        package: &str,
        script: &Path,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// The real `frida` CLI, found on `PATH` unless `program` says otherwise.
#[derive(Debug, Clone)]
pub struct FridaCli {
    pub program: PathBuf,
}

impl Default for FridaCli {
    fn default() -> Self {
        Self {
            program: PathBuf::from("frida"),
        }
    }
}

impl FridaHost for FridaCli {
    async fn spawn(&self, serial: Option<&str>, package: &str, script: &Path) -> Result<()> {
        let mut cmd = Command::new(&self.program);
        cmd.arg("-U");
        if let Some(serial) = serial {
            cmd.args(["-D", serial]);
        }
        cmd.args(["-f", package, "-l"]);
        cmd.arg(script);
        cmd.args(["--no-pause"]);
        let status = cmd.status().await.with_context(|| {
            format!(
                "running {} (is frida-tools installed?)",
                self.program.display()
            )
        })?;
        if !status.success() {
            bail!("frida {status}");
        }
        Ok(())
    }
}

/// A host temp dir holding the unpinning script, removed on drop.
struct ScriptFile(PathBuf);

impl ScriptFile {
    fn write() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("sieve-frida-{}", Uuid::new_v4()));
        fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("android-unpinning.js");
        fs::write(&path, UNPINNING_SCRIPT)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for ScriptFile {
    fn drop(&mut self) {
        if let Some(dir) = self.0.parent() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

/// Full workflow: verify the binary matches the device's ABI, push and start
/// `frida-server`, then spawn `package` with the unpinning script attached.
pub async fn run<B: AndroidBridge, H: FridaHost>(
    bridge: &B,
    host: &H,
    serial: Option<&str>,
    package: &str,
    frida_server: &Path,
) -> Result<()> {
    let abi = device_abi(bridge, serial).await?;
    check_binary_abi(frida_server, abi)?;
    push_server(bridge, serial, frida_server).await?;
    start_server(bridge, serial).await?;
    let script = ScriptFile::write()?;
    host.spawn(serial, package, &script.0).await
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn parses_known_abis() {
        assert_eq!(Abi::parse("arm64-v8a\n").unwrap(), Abi::Arm64);
        assert_eq!(Abi::parse("armeabi-v7a").unwrap(), Abi::Arm);
        assert_eq!(Abi::parse("x86_64").unwrap(), Abi::X86_64);
        assert_eq!(Abi::parse("x86").unwrap(), Abi::X86);
        assert!(Abi::parse("mips").is_err());
    }

    #[test]
    fn asset_arch_names_match_frida_release_naming() {
        assert_eq!(Abi::Arm64.frida_asset_arch(), "arm64");
        assert_eq!(Abi::Arm.frida_asset_arch(), "arm");
        assert_eq!(Abi::X86_64.frida_asset_arch(), "x86_64");
        assert_eq!(Abi::X86.frida_asset_arch(), "x86");
    }

    fn elf_stub(machine: u16) -> Vec<u8> {
        let mut bytes = vec![0u8; 24];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes
    }

    #[test]
    fn check_binary_abi_accepts_a_matching_elf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frida-server");
        fs::write(&path, elf_stub(183)).unwrap();
        check_binary_abi(&path, Abi::Arm64).unwrap();
    }

    #[test]
    fn check_binary_abi_rejects_a_mismatched_elf() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frida-server");
        fs::write(&path, elf_stub(62)).unwrap(); // x86_64
        let err = check_binary_abi(&path, Abi::Arm64).unwrap_err().to_string();
        assert!(err.contains("arm64"), "{err}");
    }

    #[test]
    fn check_binary_abi_rejects_a_non_elf_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frida-server");
        fs::write(&path, b"not an elf").unwrap();
        let err = check_binary_abi(&path, Abi::Arm64).unwrap_err().to_string();
        assert!(err.contains("ELF"), "{err}");
    }

    /// Fakes `adb` for the device-side part of the workflow.
    #[derive(Default)]
    struct MockBridge {
        abi: &'static str,
        already_root: bool,
        su_works: bool,
        becomes_running_after: Mutex<u32>,
        calls: Mutex<Vec<String>>,
    }

    impl MockBridge {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AndroidBridge for MockBridge {
        async fn adb(&self, _serial: Option<&str>, args: &[&str]) -> Result<String> {
            self.calls.lock().unwrap().push(args.join(" "));
            match args {
                ["push", _, REMOTE_SERVER] => Ok("1 file pushed\n".into()),
                ["root"] if self.already_root => bail!("adb: unexpected call"),
                ["root"] => {
                    // Treated as a no-op "already root"-equivalent path in
                    // these tests; su is used instead when root is wanted.
                    Ok("adbd cannot run as root in production builds\n".into())
                }
                ["wait-for-device"] => Ok(String::new()),
                _ => bail!("unexpected adb call: {args:?}"),
            }
        }

        async fn shell(&self, _serial: Option<&str>, command: &str) -> Result<String> {
            self.calls.lock().unwrap().push(format!("shell {command}"));
            match command {
                "getprop ro.product.cpu.abi" => Ok(format!("{}\n", self.abi)),
                "id -u" => Ok(if self.already_root { "0\n" } else { "2000\n" }.into()),
                "su -c 'id -u'" if self.su_works => Ok("0\n".into()),
                "su -c 'id -u'" => bail!("su: inaccessible or not found"),
                c if c.starts_with("chmod 755") => Ok(String::new()),
                c if c.starts_with("nohup") || c.contains("sieve-frida-server >/dev/null") => {
                    Ok(String::new())
                }
                c if c.starts_with("su -c 'nohup") => Ok(String::new()),
                "ps -A 2>/dev/null | grep -c sieve-frida-server" => {
                    let mut left = self.becomes_running_after.lock().unwrap();
                    if *left == 0 {
                        Ok("1\n".into())
                    } else {
                        *left -= 1;
                        Ok("0\n".into())
                    }
                }
                "pkill -f sieve-frida-server" => Ok(String::new()),
                other => bail!("unexpected shell call: {other:?}"),
            }
        }
    }

    /// Records each call, including the script's content read while the
    /// file is still live (the caller removes it right after `spawn`
    /// returns).
    #[derive(Default)]
    struct MockHost {
        calls: Mutex<Vec<(Option<String>, String, String)>>,
    }

    impl FridaHost for MockHost {
        async fn spawn(&self, serial: Option<&str>, package: &str, script: &Path) -> Result<()> {
            let contents = fs::read_to_string(script).unwrap();
            self.calls.lock().unwrap().push((
                serial.map(str::to_string),
                package.to_string(),
                contents,
            ));
            Ok(())
        }
    }

    #[tokio::test]
    async fn device_abi_parses_the_prop() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            ..Default::default()
        };
        assert_eq!(device_abi(&bridge, None).await.unwrap(), Abi::Arm64);
    }

    #[tokio::test]
    async fn start_server_uses_su_when_not_already_root() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            su_works: true,
            ..Default::default()
        };
        start_server(&bridge, Some("emu")).await.unwrap();
        let calls = bridge.calls();
        assert!(calls.iter().any(|c| c.starts_with("shell su -c 'nohup")));
    }

    #[tokio::test]
    async fn start_server_fails_without_root() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            ..Default::default()
        };
        let err = start_server(&bridge, Some("emu"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs root"), "{err}");
    }

    #[tokio::test]
    async fn start_server_retries_until_the_process_shows_up() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            already_root: true,
            becomes_running_after: Mutex::new(2),
            ..Default::default()
        };
        start_server(&bridge, Some("emu")).await.unwrap();
    }

    #[tokio::test]
    async fn full_run_pushes_starts_and_spawns_with_the_script() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            already_root: true,
            ..Default::default()
        };
        let host = MockHost::default();
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("frida-server");
        fs::write(&server, elf_stub(183)).unwrap();

        run(&bridge, &host, Some("emu"), "com.example.app", &server)
            .await
            .unwrap();

        let host_calls = host.calls.lock().unwrap();
        assert_eq!(host_calls.len(), 1);
        let (serial, package, script_contents) = &host_calls[0];
        assert_eq!(serial.as_deref(), Some("emu"));
        assert_eq!(package, "com.example.app");
        assert!(script_contents.contains("sieve-unpin"));
    }

    #[tokio::test]
    async fn run_rejects_a_mismatched_binary_before_pushing_it() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            ..Default::default()
        };
        let host = MockHost::default();
        let dir = tempfile::tempdir().unwrap();
        let server = dir.path().join("frida-server");
        fs::write(&server, elf_stub(3)).unwrap(); // x86

        let err = run(&bridge, &host, Some("emu"), "com.example.app", &server)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("arm64"), "{err}");
        // The ABI check reads the device's ABI (harmless) but must happen
        // before anything is pushed or started.
        assert!(!bridge.calls().iter().any(|c| c.starts_with("push")));
        assert!(host.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn is_server_running_parses_the_grep_count() {
        let bridge = MockBridge {
            abi: "arm64-v8a",
            becomes_running_after: Mutex::new(0),
            ..Default::default()
        };
        assert!(is_server_running(&bridge, None).await.unwrap());
    }
}
