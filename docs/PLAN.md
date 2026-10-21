# Sieve: Rust HTTP Debug Proxy, Full Plan

Oct 20, 2026 · @Sam

> Translated from the original Indonesian plan. The working name `rtap` has been replaced by `sieve` throughout.

## Goals & scope

The target is a single Rust binary (`sieve`) that can intercept HTTP/HTTPS from Android and iOS, with device setup that is as automated as possible and a local web UI for watching traffic live.

**Target platforms**

- Host: macOS first (the iOS Simulator needs a Mac), Linux later.
- Android: emulators and physical devices, rooted or not.
- iOS: Simulator and physical devices (non-jailbroken).

**Definition of done for v1**

- HTTPS requests from Android and iOS debug apps show up in full (headers, body, timing) in the UI.
- Android mode 1 and iOS Simulator setup each take a single command.
- A physical iOS device can be set up by scanning a QR code.

**Non-goals for v1**

- HTTP/3 (QUIC), gRPC decoding, and a VPN companion app.
- Pinning bypass for third-party apps you don't own or aren't allowed to test.
- Cloning Charles or HTTP Toolkit feature by feature; focus on the everyday debugging flow.

## Architecture & Rust stack

One `sieve` binary with three core crates: `sieve-core` holds the proxy and the CA, `sieve-device` handles per-platform setup, and `sieve-ui` serves the API and the web UI.

```
 Android / iOS device ──HTTP(S)──▶ sieve-core (CA + MITM proxy)
                                        │  Flow (tokio broadcast)
                                        ▼
                                   sieve-ui (axum API + WS + web UI) ──▶ browser

 sieve-device (adb, simctl, networksetup, mobileconfig): setup / teardown only
```

Devices only talk to `sieve-core`; every request becomes a `Flow` that is broadcast to `sieve-ui`, while `sieve-device` is only used during setup and teardown.

**Workspace layout**

```
sieve/
  Cargo.toml          # workspace
  crates/
    sieve-core/       # CA, proxy, Flow, rules
    sieve-device/     # android, ios_sim, ios_device
    sieve-ui/         # axum API + embedded frontend assets
    sieve/            # CLI binary (clap)
  ui/                 # Svelte + Vite frontend
  scripts/android/    # inject-ca.sh for mode 2
```

**Main crates**

| Need | Crate | Notes |
| --- | --- | --- |
| Async runtime | `tokio` | `full` feature |
| MITM proxy | `hudsucker` | Built on hyper + rustls |
| CA and leaf certs | `rcgen` | Generate and sign a cert per host |
| TLS | `rustls` | Used through hudsucker |
| Web API + WS | `axum` | Bind to localhost |
| CLI | `clap` | Derive API |
| Embedded UI assets | `rust-embed` | Keeps the binary a single file |
| Body decompression | `async-compression` | gzip, br, zstd |
| Terminal QR | `qrcode` | For iPhone setup |
| Logging | `tracing` + `tracing-subscriber` | Level via env |
| Config | `serde` + `toml` + `directories` | Per-OS config location |

Crate versions are checked at the start of phase 0, because the `hudsucker` and `rustls` APIs change fairly often between major versions.

## Phases 0–1: foundation, CA, and core proxy

Outcome of this phase: `sieve start` runs a proxy on port 8080 that can intercept HTTPS from `curl` on the laptop and logs every request to the terminal.

**Phase 0: project setup**

- [x] Cargo workspace with the `sieve-core`, `sieve-device`, and `sieve-ui` crates and the `sieve` binary (see Architecture).
- [x] CLI with `clap`, subcommands `start`, `ca`, `android`, `ios-sim`, `ios-device`.
- [x] Logging with `tracing`; config in `~/.config/sieve/` (or `~/Library/Application Support/sieve/` on macOS) via the `directories` crate.

**Phase 1a: Certificate Authority**

- [x] Generate the root CA once with `rcgen`: ECDSA P-256 or RSA 2048 key, valid for about 1 year, stored as `ca.pem` and `ca.key` with permission 600.
- [x] `sieve ca export --format pem|der|cer` for device needs (Android needs the hash file name, iOS needs `.cer` or a profile).
- [x] Compute `subject_hash_old` for the Android file name. Calling `openssl x509 -subject_hash_old` is fine at first; it can be replaced with a Rust implementation later.
- [x] Per-host leaf certs are generated on the fly and cached in memory (LRU); the SAN must be set to the hostname/IP.

**Phase 1b: core proxy**

- [x] Proxy built on `hudsucker` (hyper + rustls) using the CA from 1a. Done on hyper + tokio-rustls directly instead, because hudsucker cannot report client TLS handshake failures (see `proxy/mod.rs`).
- [x] Support HTTP/1.1, HTTP/2 via ALPN, and WebSocket upgrades.
- [x] Every request becomes a `Flow` (id, time, method, URL, headers, request/response body, status, duration, size) sent to a `tokio::sync::broadcast` channel.
- [x] Large bodies are stored truncated (e.g. at most 5 MB in memory); the rest is marked truncated.
- [x] Decompress gzip, br, and zstd for display, but forward the original traffic as is.
- [x] Per-host passthrough option (no interception) for pinned or unimportant domains.

**Phase 1 tests**

- [x] `curl -x http://127.0.0.1:8080 --cacert ca.pem https://example.com` succeeds and is recorded.
- [ ] A browser on the Mac with a manual proxy can open HTTP/2 sites without certificate errors.

## Phase 2: Android mode 1 + iOS Simulator

Outcome of this phase: `sieve android setup` and `sieve ios-sim setup` get debug apps intercepted straight away, with no manual step other than installing the CA once on Android.

**Android mode 1 (no root, your own app)**

- [x] Detect devices with `adb devices -l`; if there is more than one, ask the user to pick or accept `--serial`.
- [x] `adb reverse tcp:8080 tcp:8080` so the device reaches the proxy on `127.0.0.1` over USB.
- [x] `adb shell settings put global http_proxy 127.0.0.1:8080` during setup.
- [x] `sieve android teardown` restores `:0` and removes the reverse. Teardown also runs automatically on Ctrl+C.
- [x] Push the CA to `/sdcard/Download/sieve-ca.crt`, then print short instructions for installing it via Settings > Security > Install certificate.
- [x] `sieve android nsc` prints a `network_security_config.xml` snippet (a `debug-overrides` block that trusts user certs) and the manifest line to add.
- [x] Health check: proxy is set, reverse is active, and a test request from `adb shell` (if the device has `curl`) reaches the proxy.

Running `adb` through `tokio::process::Command` is enough. Wrap it in an `AndroidBridge` trait so it is easy to mock in tests.

**iOS Simulator (macOS only)**

- [x] List booted simulators with `xcrun simctl list devices booted -j`.
- [x] Install the CA: `xcrun simctl keychain <udid> add-root-cert ca.pem`.
- [x] Set the Mac system proxy with `networksetup -setwebproxy` and `-setsecurewebproxy` on the active network service. Save the old settings first.
- [x] Restore the old settings on teardown or Ctrl+C. This is mandatory, because the proxy applies to the whole Mac.
- [x] Alternative that leaves the Mac alone: document how to set the proxy in `URLSessionConfiguration` for your own app's debug build.

**Phase 2 tests**

- [ ] Sample Android (OkHttp) and iOS (URLSession) apps show HTTPS requests in the `sieve` log.
- [ ] After teardown, internet on the device and the Mac works normally again.

## Phase 3: Web UI & live traffic API

Outcome of this phase: open `http://localhost:8081` and traffic appears live, can be filtered, and each request's details can be opened.

**Backend (`sieve-ui`, using `axum`)**

- [x] `GET /api/flows` (paging, filter by host/method/status) and `GET /api/flows/:id` (full detail + body).
- [x] `GET /api/ws` WebSocket that streams summaries of new flows from the broadcast channel.
- [x] `DELETE /api/flows` to clear, `GET /api/flows/:id/har` to export HAR, and a "copy as curl" endpoint.
- [x] Frontend assets are embedded into the binary with `rust-embed`, so it stays a single file.
- [x] Bind to `127.0.0.1` only by default; expose to the LAN only with an explicit flag.

**Frontend**

- [x] Stack options: Svelte or SolidJS + Vite (lightweight), or Leptos for full Rust. Recommendation: Svelte, to get it done quickly.
- [x] Virtualized request table (thousands of rows), columns: time, method, host, path, status, size, duration.
- [x] Detail panel: Headers, Request body, Response body (pretty JSON, image preview, hex for binary), and Timing tabs.
- [x] Fast text filter plus a per-device/source filter.
- [x] Pause capture and clear buttons.

**Optional later**

- [ ] Wrap it as a desktop app with Tauri, reusing the same frontend.
- [ ] Persist sessions to SQLite (`sqlx` or `rusqlite`) so they can be reopened.

## Phase 4: physical iOS device + Android system CA

Outcome of this phase: a real iPhone can be set up by scanning a QR code, and an emulator or rooted device can intercept every app without changing app code.

**Physical iOS device (semi-automatic)**

- [x] Proxy listens on the LAN IP (opt-in), with automatic detection of the Mac's Wi-Fi IP.
- [x] `/setup/ios.mobileconfig` endpoint containing a `com.apple.security.root` payload with the CA. The profile does not need to be signed for v1 (it shows as "Not Verified", which is normal).
- [x] `/proxy.pac` endpoint that routes all traffic to `PROXY <mac-ip>:8080; DIRECT`.
- [x] `sieve ios-device setup` shows a QR code in the terminal (`qrcode` crate) pointing at the `/setup/ios` page, which has a profile download button and short steps.
- [x] Manual steps on the iPhone: install the profile, enable it under Settings > General > About > Certificate Trust Settings, set the Wi-Fi proxy to Automatic with the PAC URL.
- [x] The setup page can check status: once requests from the iPhone come through HTTPS without errors, show a green check mark.

**Android mode 2 (Google APIs emulator or rooted device)**

- [x] Detect whether `adb root` or `su` is available; if not, refuse with a clear message.
- [x] Android 13 and below: mount a tmpfs over `/system/etc/security/cacerts`, copy the existing certs plus `sieve`'s `<hash>.0`, set permissions and the SELinux label.
- [x] Android 14+: do the same in `/apex/com.android.conscrypt/cacerts`, then bind-mount into the namespace of every zygote process and already-running app with `nsenter`.
- [x] Everything goes through a single shell script that is pushed and run via `adb shell`. Study HTTP Toolkit's approach as a reference.
- [x] The effect disappears on reboot (by design, safe); provide `sieve android inject-ca` to redo it.

**Phase 4 tests**

- [ ] Safari on the iPhone and a sample iOS app are intercepted after scanning the QR code.
- [ ] Android 14 and 15 emulators: Chrome and a sample app without an NSC are still intercepted.

## Phase 5: rewrite, breakpoints, mocks, Frida, VPN mode

This phase is done as needed; each part stands alone, so the order is free.

**5a. Rules engine (rewrite + mock)**

- [ ] Rule = matcher (host, path glob/regex, method, header) + action (modify headers, replace body, replace status, delay, mock response from file, block).
- [ ] Store rules in a watched TOML/JSON file (`notify`), plus an editor in the UI.
- [ ] Map Remote: point `api.prod.com` at `localhost:3000` to test a local backend.

**5b. Breakpoints**

- [ ] Requests matching a breakpoint rule are held (using a `oneshot` channel); the UI shows an edit form, then Continue or Abort.
- [ ] Default timeout of 60 seconds so the app never hangs forever.

**5c. Frida for certificate pinning (your own apps or ones you may test)**

- [ ] Android: `sieve android frida --package <id>` pushes `frida-server` for the right ABI, starts it, then spawns the app with an unpinning script.
- [ ] Use a well-tested open source unpinning script; check its license before bundling.
- [ ] Non-jailbroken iOS: document re-signing a debug app with the Frida gadget. Not automated in v1.
- [ ] Flutter note: Flutter apps ignore the system proxy and the OS cert store, so they need Frida or a proxy set in Dart code during debugging.

**5d. VPN mode (largest, last)**

- [ ] Android: Kotlin companion app with `VpnService` that forwards TCP to the proxy. Reference: HTTP Toolkit's Android app (open source).
- [ ] iOS: an app with NetworkExtension (Packet Tunnel or App Proxy). Requires a paid Apple Developer account.

**5e. Extras**

- [x] HAR export/import, request replay, diff of two responses. (API only: `GET/POST /api/har`, `POST /api/flows/{id}/replay`, `GET /api/diff?a=&b=`; no UI controls yet.)
- [x] Scripting plugins (e.g. with `rhai`) for custom rules. (`sieve_core::script`; the proxy does not call it until the 5a rules engine lands.)

## Testing, risks & pitfalls

The biggest risk is in device setup, not in the proxy itself; have real test devices ready from phase 2.

**Testing strategy**

- Unit tests: cert generation (SAN, validity, valid chain), rule matchers, HAR parser.
- Integration tests: run the proxy + a local HTTPS server (`axum` + rustls) in the test, then send requests via `reqwest` trusting the `sieve` CA.
- Manual device tests per phase: a sample OkHttp app (Android), URLSession (iOS), and one Flutter app to check the proxy pitfall.
- Minimum matrix: Android emulator 13, 14, 15 + one physical device; latest iOS Simulator + one iPhone.

**Risks and mitigations**

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Mac system proxy not restored after a crash | Mac loses internet | Save the old state to a file, `sieve doctor --restore` |
| Android `http_proxy` left behind | Device has no internet | Automatic teardown + manual reset command in the README |
| Android 14+ changes again in a new version | Mode 2 fails | Isolate it in one script, test on every emulator release |
| App uses pinning or Flutter | No traffic visible | Diagnostic message in the UI: TLS handshake failed vs. no connection |
| CA private key leaks | Others can intercept traffic | Per-machine key, permission 600, regenerate-CA button |
| UI exposed to the LAN | Others see traffic | Bind localhost by default, LAN opt-in + token |

**Common pitfalls**

- iOS: forgetting to enable Certificate Trust Settings, so the profile is installed but HTTPS still fails.
- Android "Google Play" emulator images can't `adb root`; use a "Google APIs" image.
- Apps using HTTP/3 or QUIC can go over UDP and bypass the proxy; block UDP 443 or let them fall back.
- Leaf certs without a SAN are rejected by modern OSes even if the CN is correct.

## Model & reasoning recommendations per phase

The general pattern: Opus 5.5 with high reasoning for parts prone to design mistakes (TLS, Android system CA), Sonnet 5.5 with medium reasoning for most of the implementation, Haiku 5.5 for small repetitive work.

| Phase / work | Model | Reasoning | Why |
| --- | --- | --- | --- |
| Plan review + initial architecture design | Opus 5.5 | High | Crate structure and trait decisions are expensive to get wrong |
| Phase 1: CA + core proxy (TLS, HTTP/2, streaming bodies) | Opus 5.5 | High | rustls, ALPN, and async lifetime details are easy to get wrong |
| Phase 2: adb, simctl, networksetup wrappers | Sonnet 5.5 | Medium | Mostly glue code and subprocesses |
| Phase 3: axum API + Svelte frontend | Sonnet 5.5 | Medium | Standard CRUD and UI patterns |
| Phase 4: iOS profile, PAC, QR | Sonnet 5.5 | Medium | The formats are well defined, just assemble them |
| Phase 4: Android 14+ system CA (APEX, nsenter) | Opus 5.5 | High | Many edge cases per Android version |
| Phase 5: rules engine + breakpoints | Sonnet 5.5 | Medium–high | Breakpoint concurrency needs care |
| Debugging stubborn compile/borrow checker errors | Opus 5.5 | High | Needs longer reasoning |
| Writing tests, README, renames, small refactors | Haiku 5.5 | Low | Fast and cheap |

**Tips for executing with agents**

- Do one phase per session, and ask the agent to tick the checklist in this doc as the reference.
- After each phase, require `cargo clippy` and `cargo test` to be green before moving on.
- Fable 5.1 is the strongest, but has extra safeguards around cybersecurity; for an MITM and Frida tool like this, Opus 5.5 is more practical.
