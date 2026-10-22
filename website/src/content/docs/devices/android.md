---
title: Android
description: Route an Android emulator or device through sieve.
---

## Without root

```sh
sieve android setup       # route the device through sieve
sieve android teardown    # undo when done
```

`setup` uses `adb reverse` and sets the device's `http_proxy`, then runs the proxy and the web UI. Use `--serial` to pick a device when more than one is connected.

Install the CA from `/sdcard/Download/sieve-ca.crt` via **Settings → Security → Install certificate**.

Apps targeting Android 7+ ignore user CAs by default. For your own debug build, print the `network_security_config.xml` snippet you need:

```sh
sieve android nsc
```

Check that the device is routed to a running proxy:

```sh
sieve android health
```

## Emulator or rooted device

```sh
sieve android inject-ca
```

This installs the CA into the system store, so apps trust it without any app changes.

It requires a **Google APIs** emulator image (not Google Play) or a rooted device. It does not survive a reboot.

## Apps that ignore the proxy

Some apps bypass `http_proxy`. `sieve android vpn` routes all TCP traffic through sieve with a VPN companion app. Install the app from [`android-vpn/`](https://github.com/saamodra/sieve/blob/main/android-vpn/README.md) first.

```sh
sieve android vpn
```

## Pinned apps

If TLS handshakes fail for a host, the app probably pins its certificate. See [Certificate pinning](/guides/certificate-pinning/).
