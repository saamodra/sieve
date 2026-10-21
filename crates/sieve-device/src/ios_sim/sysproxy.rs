//! The Mac system proxy: detect the active network service, save its current
//! web and secure web proxy settings, point both at sieve, and put them back.

use std::{
    fs,
    future::Future,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::CommandRunner;

const STATE_FILE: &str = "ios-sim-proxy.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxySettings {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

/// What the network service looked like before sieve touched it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedState {
    pub service: String,
    pub web: ProxySettings,
    pub secure_web: ProxySettings,
}

#[derive(Debug)]
pub struct LeftoverState {
    pub path: PathBuf,
    pub state: SavedState,
}

pub fn state_file(state_dir: &Path) -> PathBuf {
    state_dir.join(STATE_FILE)
}

/// Parses `networksetup -getwebproxy` / `-getsecurewebproxy` output.
pub fn parse_proxy_settings(output: &str) -> Result<ProxySettings> {
    let (mut enabled, mut server, mut port) = (None, None, None);
    for line in output.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "Enabled" => enabled = Some(value.eq_ignore_ascii_case("yes")),
            "Server" => server = Some(value.to_string()),
            "Port" => port = value.parse::<u16>().ok(),
            _ => {}
        }
    }
    match (enabled, server, port) {
        (Some(enabled), Some(server), Some(port)) => Ok(ProxySettings {
            enabled,
            server,
            port,
        }),
        _ => bail!("unrecognised networksetup output: {output:?}"),
    }
}

/// Finds the service whose `Device:` is `device` in
/// `networksetup -listnetworkserviceorder` output.
pub fn parse_service_for_device(output: &str, device: &str) -> Option<String> {
    let mut name: Option<&str> = None;
    for line in output.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix('(') {
            // "(1) Wi-Fi" or "(*) Disabled service"; the hardware port line
            // starts with "(Hardware Port:" instead.
            if let Some((tag, svc)) = rest.split_once(") ")
                && (tag == "*" || tag.chars().all(|c| c.is_ascii_digit()))
            {
                name = Some(svc);
                continue;
            }
            if rest.starts_with("Hardware Port:")
                && rest
                    .split("Device:")
                    .nth(1)
                    .is_some_and(|d| d.trim().trim_end_matches(')').trim() == device)
            {
                return name.map(str::to_string);
            }
        }
    }
    None
}

/// Default route interface mapped through the network service order.
pub fn active_service(runner: &dyn CommandRunner) -> Result<String> {
    let route = runner.run("route", &["-n", "get", "default"])?;
    let device = route
        .lines()
        .find_map(|l| l.trim().strip_prefix("interface:"))
        .map(|d| d.trim().to_string())
        .ok_or_else(|| anyhow!("no default route; is the Mac online?"))?;
    let order = runner.run("networksetup", &["-listnetworkserviceorder"])?;
    parse_service_for_device(&order, &device)
        .ok_or_else(|| anyhow!("no network service found for interface {device}"))
}

fn read_state(runner: &dyn CommandRunner, service: &str) -> Result<SavedState> {
    Ok(SavedState {
        service: service.to_string(),
        web: parse_proxy_settings(&runner.run("networksetup", &["-getwebproxy", service])?)?,
        secure_web: parse_proxy_settings(
            &runner.run("networksetup", &["-getsecurewebproxy", service])?,
        )?,
    })
}

/// Saves the current settings to the state file, then points the web and
/// secure web proxy of the active service at `127.0.0.1:port`. The save
/// happens first so a crash at any later point is recoverable. A failure
/// while setting restores what was saved.
pub fn apply(runner: &dyn CommandRunner, state_dir: &Path, port: u16) -> Result<SavedState> {
    let file = state_file(state_dir);
    if file.exists() {
        bail!(
            "{} exists: a previous run did not restore the Mac proxy. \
             Run `sieve ios-sim teardown` or `sieve doctor --restore` first",
            file.display()
        );
    }
    let service = active_service(runner)?;
    let state = read_state(runner, &service)?;
    fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    fs::write(&file, serde_json::to_vec_pretty(&state)?)
        .with_context(|| format!("writing {}", file.display()))?;

    let port_arg = port.to_string();
    let set = || -> Result<()> {
        runner.run(
            "networksetup",
            &["-setwebproxy", &service, "127.0.0.1", &port_arg],
        )?;
        runner.run(
            "networksetup",
            &["-setsecurewebproxy", &service, "127.0.0.1", &port_arg],
        )?;
        Ok(())
    };
    if let Err(e) = set() {
        if let Err(re) = restore(runner, &file) {
            warn!("restoring after failed proxy setup also failed: {re:#}");
        }
        return Err(e);
    }
    info!(service = %service, port, "Mac system proxy set");
    Ok(state)
}

fn restore_one(
    runner: &dyn CommandRunner,
    service: &str,
    set_cmd: &str,
    state_cmd: &str,
    old: &ProxySettings,
) -> Result<()> {
    // Put the old server back first (this also switches the proxy on), then
    // switch it off again if it was off before.
    if !old.server.is_empty() && old.port != 0 {
        runner.run(
            "networksetup",
            &[set_cmd, service, &old.server, &old.port.to_string()],
        )?;
    }
    if !old.enabled {
        runner.run("networksetup", &[state_cmd, service, "off"])?;
    }
    Ok(())
}

/// Restores the settings from a state file and deletes it. The file is kept
/// if restoring fails, so it can be retried.
pub fn restore(runner: &dyn CommandRunner, file: &Path) -> Result<()> {
    let state = load_state(file)?;
    let s = &state.service;
    restore_one(runner, s, "-setwebproxy", "-setwebproxystate", &state.web)?;
    restore_one(
        runner,
        s,
        "-setsecurewebproxy",
        "-setsecurewebproxystate",
        &state.secure_web,
    )?;
    fs::remove_file(file).with_context(|| format!("removing {}", file.display()))?;
    info!(service = %s, "Mac system proxy restored");
    Ok(())
}

fn load_state(file: &Path) -> Result<SavedState> {
    let text = fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", file.display()))
}

/// The saved state a crashed run left behind, if any.
pub fn find_leftover(state_dir: &Path) -> Result<Option<LeftoverState>> {
    let path = state_file(state_dir);
    if !path.exists() {
        return Ok(None);
    }
    let state = load_state(&path)?;
    Ok(Some(LeftoverState { path, state }))
}

/// Sets the system proxy, awaits `session`, and restores the old settings
/// however the session ended.
pub async fn run_guarded<F>(
    runner: &dyn CommandRunner,
    state_dir: &Path,
    port: u16,
    session: F,
) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    apply(runner, state_dir, port)?;
    let result = session.await;
    let restored = restore(runner, &state_file(state_dir));
    match (result, restored) {
        (Ok(()), restored) => restored,
        (Err(e), Ok(())) => Err(e),
        (Err(e), Err(re)) => Err(e.context(format!("also failed to restore the proxy: {re:#}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::super::runner::mock::MockRunner;
    use super::*;

    const ORDER: &str = "An asterisk (*) denotes that a network service is disabled.\n\
(1) USB 10/100 LAN\n(Hardware Port: USB 10/100 LAN, Device: en7)\n\n\
(*) Old Thing\n(Hardware Port: Old, Device: en3)\n\n\
(3) Wi-Fi\n(Hardware Port: Wi-Fi, Device: en0)\n";
    const OFF: &str = "Enabled: No\nServer: \nPort: 0\nAuthenticated Proxy Enabled: 0\n";
    const ON: &str =
        "Enabled: Yes\nServer: proxy.corp\nPort: 3128\nAuthenticated Proxy Enabled: 0\n";

    fn runner(web: &str, secure: &str) -> MockRunner {
        MockRunner::default()
            .reply(
                "route -n get default",
                "   route to: default\n  interface: en0\n",
            )
            .reply("networksetup -listnetworkserviceorder", ORDER)
            .reply("networksetup -getwebproxy Wi-Fi", web)
            .reply("networksetup -getsecurewebproxy Wi-Fi", secure)
    }

    #[test]
    fn parses_enabled_and_disabled() {
        let on = parse_proxy_settings(ON).unwrap();
        assert_eq!(
            (on.enabled, on.server.as_str(), on.port),
            (true, "proxy.corp", 3128)
        );
        let off = parse_proxy_settings(OFF).unwrap();
        assert_eq!((off.enabled, off.server.as_str(), off.port), (false, "", 0));
        assert!(parse_proxy_settings("garbage").is_err());
    }

    #[test]
    fn maps_device_to_service() {
        assert_eq!(
            parse_service_for_device(ORDER, "en0").as_deref(),
            Some("Wi-Fi")
        );
        assert_eq!(
            parse_service_for_device(ORDER, "en7").as_deref(),
            Some("USB 10/100 LAN")
        );
        assert_eq!(
            parse_service_for_device(ORDER, "en3").as_deref(),
            Some("Old Thing")
        );
        assert_eq!(parse_service_for_device(ORDER, "en9"), None);
    }

    #[test]
    fn detects_active_service() {
        assert_eq!(active_service(&runner(OFF, OFF)).unwrap(), "Wi-Fi");
    }

    #[test]
    fn saves_before_setting() {
        let dir = tempfile::tempdir().unwrap();
        let r = runner(ON, OFF);
        let saved = apply(&r, dir.path(), 8080).unwrap();
        assert_eq!(saved.web.server, "proxy.corp");
        // state on disk matches
        let leftover = find_leftover(dir.path()).unwrap().unwrap();
        assert_eq!(leftover.state, saved);
        let calls = r.calls();
        let last_get = calls.iter().rposition(|c| c.contains(" -get")).unwrap();
        let first_set = calls.iter().position(|c| c.contains(" -set")).unwrap();
        assert!(last_get < first_set);
        assert_eq!(
            r.mutations(),
            vec![
                "networksetup -setwebproxy Wi-Fi 127.0.0.1 8080",
                "networksetup -setsecurewebproxy Wi-Fi 127.0.0.1 8080",
            ]
        );
    }

    #[test]
    fn refuses_to_overwrite_existing_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(state_file(dir.path()), "{}").unwrap();
        let r = runner(OFF, OFF);
        assert!(apply(&r, dir.path(), 8080).is_err());
        assert!(r.mutations().is_empty());
    }

    #[test]
    fn failed_set_restores() {
        let dir = tempfile::tempdir().unwrap();
        let r = runner(OFF, OFF).fail("networksetup -setsecurewebproxy Wi-Fi 127");
        assert!(apply(&r, dir.path(), 8080).is_err());
        assert!(find_leftover(dir.path()).unwrap().is_none());
        assert!(
            r.mutations()
                .contains(&"networksetup -setwebproxystate Wi-Fi off".to_string())
        );
    }

    #[tokio::test]
    async fn set_session_restore_order() {
        let dir = tempfile::tempdir().unwrap();
        let r = runner(ON, OFF);
        run_guarded(&r, dir.path(), 9000, async { Ok(()) })
            .await
            .unwrap();
        assert_eq!(
            r.mutations(),
            vec![
                "networksetup -setwebproxy Wi-Fi 127.0.0.1 9000",
                "networksetup -setsecurewebproxy Wi-Fi 127.0.0.1 9000",
                // web was on with its own server: put back, stays enabled
                "networksetup -setwebproxy Wi-Fi proxy.corp 3128",
                // secure web was off with no server: just switch off
                "networksetup -setsecurewebproxystate Wi-Fi off",
            ]
        );
        assert!(find_leftover(dir.path()).unwrap().is_none());
    }

    #[tokio::test]
    async fn restores_after_failing_session() {
        let dir = tempfile::tempdir().unwrap();
        let r = runner(OFF, OFF);
        let err = run_guarded(&r, dir.path(), 8080, async { bail!("proxy died") })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("proxy died"));
        let m = r.mutations();
        assert_eq!(m[2], "networksetup -setwebproxystate Wi-Fi off");
        assert_eq!(m[3], "networksetup -setsecurewebproxystate Wi-Fi off");
        assert!(find_leftover(dir.path()).unwrap().is_none());
    }

    #[test]
    fn restores_from_existing_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = SavedState {
            service: "Wi-Fi".into(),
            web: parse_proxy_settings(ON).unwrap(),
            secure_web: parse_proxy_settings(OFF).unwrap(),
        };
        let file = state_file(dir.path());
        fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
        let r = MockRunner::default();
        restore(&r, &file).unwrap();
        assert_eq!(
            r.mutations(),
            vec![
                "networksetup -setwebproxy Wi-Fi proxy.corp 3128",
                "networksetup -setsecurewebproxystate Wi-Fi off",
            ]
        );
        assert!(!file.exists());
    }

    #[test]
    fn failed_restore_keeps_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let state = SavedState {
            service: "Wi-Fi".into(),
            web: parse_proxy_settings(OFF).unwrap(),
            secure_web: parse_proxy_settings(OFF).unwrap(),
        };
        let file = state_file(dir.path());
        fs::write(&file, serde_json::to_vec(&state).unwrap()).unwrap();
        let r = MockRunner::default().fail("networksetup");
        assert!(restore(&r, &file).is_err());
        assert!(file.exists());
    }

    #[test]
    fn leftover_detection() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_leftover(dir.path()).unwrap().is_none());
        fs::write(state_file(dir.path()), "not json").unwrap();
        assert!(find_leftover(dir.path()).is_err());
    }

    /// Read-only detection against this Mac: `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the real network configuration"]
    fn real_detection() {
        let service = active_service(&super::super::SystemRunner).unwrap();
        println!("active service: {service}");
    }
}
