# sieve

sieve is an HTTP and HTTPS debug proxy for Android and iOS, written in Rust. It runs on your Mac, terminates TLS for the devices you point at it using its own root certificate authority, and records every request as a flow. A local web UI shows the flows as they arrive. The target is a single binary with device setup automated as far as the platform allows, so that a debug app on an emulator, a Simulator, or a phone can be inspected without changing its code.

## Status

sieve is pre-alpha and built in phases. The full plan, with a checklist for each phase, is in [docs/PLAN.md](docs/PLAN.md).

Phases 0 to 4 are implemented: the certificate authority, the proxy (HTTP/1.1, HTTP/2, WebSocket), the web UI, and device setup for Android (modes 1 and 2), the iOS Simulator, and physical iPhones. Device setup has not yet been verified on real devices; [docs/TESTING.md](docs/TESTING.md) is the manual test guide. Phase 5 (rules, breakpoints, Frida, VPN mode) is not started.

## Build from source

You need a Rust toolchain (stable, with `cargo`). The current crates have no system dependencies.

```sh
cargo build --release
```

The binary is written to `target/release/sieve`. You can also run it without installing:

```sh
cargo run -q -- --help
```

The web UI lives in `ui/`, which is a Svelte and Vite project. Building it requires Node.js and npm. Build it before `cargo build` so its assets are embedded in the binary:

```sh
cd ui
npm ci
npm run build
```

## Quick start

sieve keeps its root certificate authority in its config directory. Print the location of its files with:

```sh
sieve ca path
```

Start the proxy and the web UI:

```sh
sieve start
```

By default the proxy listens on `127.0.0.1:8080` and the web UI on `http://localhost:8081`. Both addresses can be changed with `--listen` and `--ui-listen`, and `--passthrough <HOST>` tunnels a host without intercepting it. `--upstream-ca <PEM>` (repeatable, or `upstream_ca_files` under `[proxy]` in `config.toml`) trusts extra roots when verifying upstream servers, such as a local backend with its own CA. Requests appear in the web UI as they arrive.

To trust the CA on a device, export it:

```sh
sieve ca export --format pem -o sieve-ca.pem
```

The `--format` option accepts `pem`, `der`, and `cer`. Leaving out `-o` writes the certificate to standard output. The device setup commands below export and install the certificate for you where they can.

## Device setup

Each platform has its own setup command. Run `sieve --help` or `sieve <command> --help` to see the options for each one.

### Android, mode 1 (no root, your own debug app)

`sieve android setup` routes the device through sieve over USB. It runs `adb reverse` so the device can reach the proxy on `127.0.0.1:8080`, sets the device's `http_proxy` to the proxy, and starts the proxy. Use `--serial <SERIAL>` when more than one device is connected; serials are listed by `adb devices`.

Install the CA on the device once, through Settings, Security, Install certificate. The setup command pushes the file to `/sdcard/Download/sieve-ca.crt`.

Apps that target Android 7 or later do not trust user-installed certificates unless they opt in. For debug builds of your own app, `sieve android nsc` prints a `network_security_config.xml` snippet with a `debug-overrides` block that trusts user certificates, and the manifest line to add. Use it only for builds you control.

Run `sieve android teardown` when you are done. It clears `http_proxy` and removes the `adb reverse` rule.

### Android, mode 2 (emulator or rooted device)

`sieve android inject-ca` installs the CA into the system certificate store, so apps that trust the system store will accept it without changes to their code. It requires either a Google APIs emulator image or a rooted device, because the store is not writable otherwise. Google Play images do not allow `adb root`. Injection is not persistent: it is lost on reboot, so run the command again after a restart.

### iOS Simulator (macOS only)

`sieve ios-sim setup` trusts the CA in every booted Simulator, sets the Mac's system web proxy and secure web proxy on the active network service, and starts the proxy. The Mac system proxy applies to the whole machine, so every app on the Mac sends its traffic through sieve while it runs, but only connections from inside the Simulator are intercepted. The Mac's own apps are tunneled untouched and keep working; pass `--intercept-mac` to intercept them too. The previous proxy settings are saved and restored by `sieve ios-sim teardown`. If the restore does not happen, see [Reset if something is left behind](#reset-if-something-is-left-behind).

### Physical iPhone

`sieve ios-device setup` listens on the local network and prints a QR code. Scan it with the iPhone's camera to open the setup page, which offers the configuration profile (containing the CA) and a proxy auto-configuration URL. The profile is not signed, so iOS shows it as not verified; that is expected. Then:

1. Install the profile from Settings.
2. Enable it under Settings, General, About, Certificate Trust Settings. The profile has no effect on HTTPS until this step is done.
3. Set the Wi-Fi proxy to Automatic and enter the proxy auto-configuration URL shown on the setup page.

The setup page shows a green check once HTTPS traffic from the phone arrives. The phone and the Mac must be on the same network.

## Reset if something is left behind

Normally the teardown commands and Ctrl+C restore everything sieve changed. If sieve exits unexpectedly, or a setup was interrupted, run the matching reset commands below.

Android, device proxy and USB routing:

```sh
adb shell settings put global http_proxy :0
adb reverse --remove-all
```

The first command clears the device's proxy setting. The second removes every `adb reverse` rule on all devices, so only use it when you do not need other reverse rules.

Mac system proxy, which is changed by `sieve ios-sim setup`:

```sh
networksetup -listallnetworkservices
networksetup -setwebproxystate <service> off
networksetup -setsecurewebproxystate <service> off
```

Replace `<service>` with the network service you use, such as `Wi-Fi`. The first command lists the names.

sieve also saves the proxy settings it replaces: the Mac's, and each Android device's previous `http_proxy`. `sieve android teardown` puts the device's old value back, not just `:0`. To see what a crashed run left behind, or to restore it, run:

```sh
sieve doctor            # list leftover state
sieve doctor --restore
```

`sieve doctor --restore` restores the Mac proxy settings and the Android `http_proxy` values saved by a setup that did not tear down. An Android device must be connected for its value to be restored; otherwise it is skipped and its state file is kept for the next run. It does not remove `adb reverse` rules; use `sieve android teardown` or the manual commands above for that. Use the manual commands above if it does not restore the settings you expect.

## Troubleshooting

**The iPhone shows the profile as installed, but HTTPS fails.** Certificate Trust Settings was not enabled for the profile. Enable it under Settings, General, About, Certificate Trust Settings.

**The Android device has no internet after a session.** The device's `http_proxy` is still set. Run `adb shell settings put global http_proxy :0` (see [Reset if something is left behind](#reset-if-something-is-left-behind)).

**An Android emulator cannot run `adb root`.** The image is a Google Play image. Use a Google APIs image instead, which allows root for system certificate injection.

**Certificate pinning.** An app that pins its certificates rejects the sieve certificate, and its requests fail even when the CA is trusted. sieve does not bypass pinning for apps you do not own. A TLS handshake failure in sieve for a host usually means pinning or an untrusted CA, not a missing connection. Use `--passthrough <HOST>` to leave a host alone when it is unrelated to your debugging. Bypassing pinning with Frida is planned for Phase 5 and is limited to apps you own or are authorized to test.

**HTTP/3 and QUIC.** Some apps send traffic over UDP using HTTP/3 (QUIC), which bypasses the proxy. sieve does not intercept QUIC. Block UDP port 443 on the device or network so the app falls back to HTTP/2 or HTTP/1.1 over TCP. HTTP/3 support is out of scope for v1.

**Flutter apps ignore the system proxy.** Flutter's networking stack does not use the system proxy or the operating system certificate store. Traffic from a Flutter app does not appear in sieve unless the app is configured to use the proxy in its Dart code during debugging, or a Frida-based approach is used (planned, Phase 5).

**Leaf certificates without a SAN.** Modern operating systems reject a leaf certificate that has no Subject Alternative Name, even when the Common Name matches. sieve issues leaf certificates with the host in the SAN.

## Responsible use

Use sieve only on apps and devices that you own or are explicitly authorized to test. Intercepting traffic from another person's device, or from an app whose owner has not agreed to it, is not something this tool is for. Installing a root CA on a device gives the holder of its private key the ability to read that device's HTTPS traffic, so keep the CA key private (its file is created with permission 600) and regenerate the CA with `sieve ca regenerate` if it may have been exposed.

## Development

Contributor guidance, the crate layout, and the cross-crate contracts are in [AGENTS.md](AGENTS.md). Before every commit, run the three verification commands:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Automated tests must not change host or device state, and must bind test servers to `127.0.0.1` on port 0. See AGENTS.md for the details.
