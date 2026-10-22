---
title: CLI reference
description: Every sieve command and flag.
---

Run `sieve <command> --help` for the authoritative list on your build.

## `sieve start`

Run the proxy and the web UI.

| Flag                 | Purpose                                                                        |
| -------------------- | ------------------------------------------------------------------------------ |
| `--listen`           | Proxy listen address (default from config, `127.0.0.1:8730`)                   |
| `--ui-listen`        | Web UI listen address (default from config, `127.0.0.1:8731`)                  |
| `--passthrough HOST` | Tunnel this host without interception. Repeatable; `*.example.com` matches every subdomain |
| `--max-body-bytes N` | Keep at most this many bytes of each body (default 5 MiB)                      |
| `--upstream-ca PEM`  | Trust the roots in this PEM file when verifying upstream servers. Repeatable   |

## `sieve ca`

Manage the root certificate authority.

| Command                                  | Purpose                                              |
| ---------------------------------------- | ---------------------------------------------------- |
| `sieve ca export [--format pem\|der\|cer] [-o FILE]` | Write the CA certificate in a device-friendly format. DER and CER need `-o` when stdout is a terminal |
| `sieve ca hash`                          | Print the CA's `subject_hash_old` (the Android cacerts file name) |
| `sieve ca info`                          | Show the CA's fingerprint, validity, and file locations |
| `sieve ca path`                          | Print where the CA files live                        |
| `sieve ca regenerate`                    | Replace the CA with a new one. Devices must trust it again |

## `sieve android`

All subcommands accept `--serial <SERIAL>`, as listed by `adb devices`.

| Command                                              | Purpose                                                          |
| ---------------------------------------------------- | ---------------------------------------------------------------- |
| `sieve android setup`                                | Route the device through sieve (no root) and run the proxy       |
| `sieve android teardown`                             | Restore the previous `http_proxy` and remove `adb reverse`       |
| `sieve android nsc`                                  | Print the `network_security_config.xml` snippet for debug builds |
| `sieve android health`                               | Check that the device is routed to a running sieve proxy         |
| `sieve android inject-ca`                            | Inject the CA into the system store (emulator or root)           |
| `sieve android frida <PACKAGE> --frida-server PATH`  | Start frida-server (needs root) and spawn the app with the unpinning script |
| `sieve android vpn`                                  | Route all TCP through sieve with the VPN companion app           |

## `sieve ios-sim`

macOS only.

| Command                                | Purpose                                                                                       |
| -------------------------------------- | --------------------------------------------------------------------------------------------- |
| `sieve ios-sim setup [--intercept-mac]` | Trust the CA in booted simulators, set the Mac proxy, and run the proxy. `--intercept-mac` also intercepts the Mac's own apps |
| `sieve ios-sim teardown`               | Restore the Mac's previous proxy settings                                                     |

## `sieve ios-device`

| Command                   | Purpose                                                        |
| ------------------------- | -------------------------------------------------------------- |
| `sieve ios-device setup`  | Listen on the LAN and show a QR code for the iPhone setup page |

## `sieve doctor`

Diagnose leftover proxy state.

| Flag        | Purpose                                                              |
| ----------- | -------------------------------------------------------------------- |
| `--restore` | Restore proxy settings saved by a setup that did not tear down       |
