---
title: Troubleshooting
description: Fixes for the most common problems.
---

| Problem                                   | Fix                                                                       |
| ----------------------------------------- | ------------------------------------------------------------------------- |
| iPhone profile installed but HTTPS fails  | Enable it in **Certificate Trust Settings**                               |
| Android has no internet after a session   | `sieve doctor --restore`, or clear `http_proxy` ([manual reset](/guides/cleanup/)) |
| `adb root` fails on the emulator          | Use a Google APIs image, not Google Play                                  |
| TLS handshake fails for a host            | Usually certificate pinning. Use `sieve android frida` or `--passthrough` |
| App traffic is missing entirely           | It may use HTTP/3 (block UDP 443) or ignore the proxy (use VPN mode)      |
| Flutter app traffic is missing            | Flutter ignores the system proxy. See [Certificate pinning](/guides/certificate-pinning/#flutter-apps) |
