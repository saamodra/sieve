---
title: Certificate pinning
description: Bypass certificate pinning on Android with Frida, and handle Flutter apps.
---

Use this only for your own apps, or apps you have permission to test. It is for pinned TLS, not for apps that already show up fine under `sieve android setup` or `sieve ios-sim setup`.

## Android: `sieve android frida`

```sh
sieve android frida com.example.app --frida-server /path/to/frida-server
```

This:

1. Reads the device's ABI (`getprop ro.product.cpu.abi`) and checks that the given `frida-server` binary's ELF header matches it, before pushing anything.
2. Pushes it to `/data/local/tmp/sieve-frida-server` and makes it executable.
3. Starts it as root (`adb root`, falling back to `su -c`) and waits for it to show up in the process list.
4. Runs the host-side `frida` CLI with sieve's bundled certificate-pinning bypass script, spawning the app suspended and resuming it once the script is loaded.

### Requirements

- Root on the device: a "Google APIs" emulator image (not "Google Play"), or a rooted physical device. Same constraint as `sieve android inject-ca`.
- A `frida-server` binary for the device's ABI, downloaded from [frida/frida releases](https://github.com/frida/frida/releases). It is not bundled, because it must match the `frida` version installed on your machine.
- `frida-tools` on the host (`pip install frida-tools`), so the `frida` CLI is on `PATH`.

### The unpinning script

sieve's script hooks the checks apps most commonly use to pin or validate certificates: `SSLContext.init` and custom `X509TrustManager`s, OkHttp 3/4's `CertificatePinner`, Android's Network Security Config pin set checks, and `WebViewClient.onReceivedSslError`. It was written from scratch for this project.

## Non-jailbroken iOS

There is no automated equivalent yet. The Frida gadget has to be injected into the app's `.ipa` and the result re-signed with your own signing identity. The [full notes](https://github.com/saamodra/sieve/blob/main/docs/frida.md) in the repository list the steps.

## Flutter apps

Flutter bundles its own TLS stack, so it ignores both the system proxy and the OS certificate store. `sieve android setup` and `sieve ios-sim setup` alone will not show Flutter traffic.

During development, point `HttpClient` at sieve explicitly in debug builds:

```dart
import 'dart:io';

HttpOverrides.global = _SieveHttpOverrides();

class _SieveHttpOverrides extends HttpOverrides {
  @override
  HttpClient createHttpClient(SecurityContext? context) {
    final client = super.createHttpClient(context)
      ..findProxy = (uri) => 'PROXY 127.0.0.1:8730'
      ..badCertificateCallback = (cert, host, port) => true; // sieve's CA
    return client;
  }
}
```

Wrap this so it only runs in debug (`kDebugMode` from `package:flutter/foundation.dart`).

For a built APK or IPA you can't change, use `sieve android frida` with a Flutter-aware pinning bypass. The bundled script does not cover Flutter's own pinning.
