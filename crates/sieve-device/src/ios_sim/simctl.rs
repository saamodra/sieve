use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::CommandRunner;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Simulator {
    pub udid: String,
    pub name: String,
}

#[derive(Deserialize)]
struct DeviceList {
    devices: std::collections::BTreeMap<String, Vec<Device>>,
}

#[derive(Deserialize)]
struct Device {
    udid: String,
    name: String,
    #[serde(default)]
    state: String,
}

/// Parses `xcrun simctl list devices booted -j`.
pub fn parse_booted(json: &str) -> Result<Vec<Simulator>> {
    let list: DeviceList = serde_json::from_str(json).context("parsing simctl device list")?;
    Ok(list
        .devices
        .into_values()
        .flatten()
        .filter(|d| d.state.is_empty() || d.state == "Booted")
        .map(|d| Simulator {
            udid: d.udid,
            name: d.name,
        })
        .collect())
}

pub fn booted_simulators(runner: &dyn CommandRunner) -> Result<Vec<Simulator>> {
    parse_booted(&runner.run("xcrun", &["simctl", "list", "devices", "booted", "-j"])?)
}

/// `xcrun simctl keychain <udid> add-root-cert <ca.pem>`
pub fn install_ca(runner: &dyn CommandRunner, udid: &str, ca_pem: &Path) -> Result<()> {
    let pem = ca_pem.to_string_lossy();
    runner.run(
        "xcrun",
        &["simctl", "keychain", udid, "add-root-cert", &pem],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::runner::mock::MockRunner;
    use super::*;

    fn device(udid: &str, name: &str) -> String {
        format!(r#"{{"udid":"{udid}","name":"{name}","state":"Booted"}}"#)
    }

    #[test]
    fn zero_booted() {
        let json = r#"{"devices":{"com.apple.CoreSimulator.SimRuntime.iOS-26-5":[]}}"#;
        assert!(parse_booted(json).unwrap().is_empty());
    }

    #[test]
    fn one_booted() {
        let json = format!(
            r#"{{"devices":{{"rt-a":[{}],"rt-b":[]}}}}"#,
            device("U1", "iPhone 16")
        );
        assert_eq!(
            parse_booted(&json).unwrap(),
            vec![Simulator {
                udid: "U1".into(),
                name: "iPhone 16".into()
            }]
        );
    }

    #[test]
    fn two_booted_across_runtimes() {
        let json = format!(
            r#"{{"devices":{{"rt-a":[{}],"rt-b":[{}]}}}}"#,
            device("U1", "iPhone 16"),
            device("U2", "iPad")
        );
        let sims = parse_booted(&json).unwrap();
        assert_eq!(sims.len(), 2);
        assert_eq!(sims[1].udid, "U2");
    }

    #[test]
    fn bad_json_is_an_error() {
        assert!(parse_booted("not json").is_err());
    }

    #[test]
    fn installs_with_simctl_keychain() {
        let r = MockRunner::default();
        install_ca(&r, "U1", Path::new("/tmp/ca.pem")).unwrap();
        assert_eq!(
            r.calls(),
            vec!["xcrun simctl keychain U1 add-root-cert /tmp/ca.pem"]
        );
    }
}
