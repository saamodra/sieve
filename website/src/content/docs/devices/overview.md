---
title: Device setup overview
description: Which sieve command to use for each kind of device.
---

Device setup is automated wherever the platform allows. Pick the row that matches your device.

| Platform                            | Command                   | Notes                                                |
| ----------------------------------- | ------------------------- | ---------------------------------------------------- |
| Android, no root                    | `sieve android setup`     | Uses `adb reverse` and `http_proxy`                  |
| Android, emulator or rooted         | `sieve android inject-ca` | Installs the CA into the system store                |
| Android, apps that ignore the proxy | `sieve android vpn`       | Needs the [companion app](https://github.com/saamodra/sieve/blob/main/android-vpn/README.md) |
| Android, pinned apps                | `sieve android frida`     | See [Certificate pinning](/guides/certificate-pinning/) |
| iOS Simulator (macOS)               | `sieve ios-sim setup`     | See [iOS Simulator](/devices/ios-simulator/)         |
| iPhone                              | `sieve ios-device setup`  | Scan the QR code on the same Wi-Fi network           |

Run `sieve <command> --help` for all options.

Every setup command restores what it changed when the session ends. If a session crashed, see [Cleanup](/guides/cleanup/).
