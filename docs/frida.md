# Frida: bypassing certificate pinning

Only for your own apps, or apps you have permission to test (see the
non-goals in [docs/PLAN.md](PLAN.md)). This is for pinned TLS, not for
apps that already show up fine under `sieve android setup` or
`sieve ios-sim setup`.

## Android: `sieve android frida`

```sh
sieve android frida --package com.example.app --frida-server /path/to/frida-server
```

This:

1. Reads the device's ABI (`getprop ro.product.cpu.abi`) and checks the
   given `frida-server` binary's ELF header matches it, before pushing
   anything.
2. Pushes it to `/data/local/tmp/sieve-frida-server` and makes it executable.
3. Starts it as root (`adb root`, falling back to `su -c`) and waits for it
   to show up in the process list.
4. Runs the host-side `frida` CLI (`frida -U -f <package> -l
   android-unpinning.js --no-pause`) with sieve's bundled certificate-pinning
   bypass script, spawning the app suspended and resuming it once the script
   is loaded.

### Requirements

- Root on the device: a "Google APIs" emulator image (not "Google Play"),
  or a rooted physical device. Same constraint as `sieve android inject-ca`.
- A `frida-server` binary for the device's ABI, downloaded from
  [frida/frida releases](https://github.com/frida/frida/releases). It is
  not bundled with sieve (it's tens of MB per ABI and must match the
  `frida` version installed on your machine, so a bundled copy would go
  stale).
- `frida-tools` installed on the host (`pip install frida-tools`), so the
  `frida` CLI is on `PATH`.

### The unpinning script

`scripts/frida/android-unpinning.js` hooks the checks apps most commonly use
to pin or validate certificates: `SSLContext.init` / any custom
`X509TrustManager`, OkHttp 3/4's `CertificatePinner`, Android's Network
Security Config pin set checks, and `WebViewClient.onReceivedSslError`.

**License note.** Several well-known community scripts exist for this, but
none were suitable to vendor as-is:

- HTTP Toolkit's `android-certificate-unpinning.js`
  ([httptoolkit/frida-interception-and-unpinning](https://github.com/httptoolkit/frida-interception-and-unpinning))
  is licensed AGPL-3.0, which is viral and does not mix cleanly into a
  differently-licensed tool.
- The Frida CodeShare "multiple unpinning" script (`@akabe1`) and the
  popular `iddoeldor/frida-snippets` collection carry no stated license at
  all, so bundling their exact code would not come with any actual grant
  of rights to redistribute.

sieve's script was written from scratch for this project, reimplementing
the same publicly documented hook points rather than copying any of the
above. If a clearly-licensed (MIT/BSD/Apache) community script becomes
available later, it's worth revisiting.

## Non-jailbroken iOS: re-signing with the Frida gadget

There's no non-jailbroken equivalent of `sieve android frida` yet (not
automated in v1 — see Phase 5c in [docs/PLAN.md](PLAN.md)). On a
non-jailbroken device, the Frida gadget has to be injected into the app's
`.ipa` and the result re-signed. Rough steps, for an app you have the
`.ipa` for (your own build, or one you're permitted to test):

1. Get `frida-tools` and `objection` (`pip install frida-tools objection`),
   or do it by hand with `optool`/`insert_dylib` and `codesign`.
2. Unpack the `.ipa` (it's a zip) and locate the main executable in
   `Payload/<App>.app/`.
3. Patch it to load the Frida gadget, either via `objection patchipa` (which
   automates steps 2-5) or manually with `optool install -c load -p
   "FridaGadget.dylib" -t <executable>` after copying
   `FridaGadget.dylib` from the [frida/frida releases](https://github.com/frida/frida/releases)
   (`frida-gadget-<version>-ios-universal.dylib.xz`) into the app bundle.
4. Repackage the `.app` into an `.ipa`.
5. Re-sign it with your own development certificate and a provisioning
   profile that covers the app's bundle id and entitlements:
   `codesign -f -s "<your signing identity>" --entitlements
   entitlements.plist Payload/<App>.app`, then re-zip as `.ipa`.
6. Install it on the device (`ios-deploy`, Xcode, or a sideloading tool).
   On launch, the gadget listens on `localhost:27042` by default; connect
   with `frida -H localhost:27042 -l android-unpinning.js` (the hook points
   are the same on iOS once you swap in an iOS pinning-bypass script — the
   bundled script here targets Android's Java runtime and does not apply).

This needs your own Apple Developer signing identity (free or paid) and
only works on apps you can legitimately re-sign and install. It is not
wired into the `sieve` CLI.

## Flutter apps don't use the system proxy or cert store

Flutter apps bundle their own TLS stack (BoringSSL via `dart:io`'s
`HttpClient`), so:

- `sieve android setup` / `sieve ios-sim setup`'s proxy settings are
  ignored. Flutter does not read the OS HTTP proxy.
- Installing the sieve CA (user or system store) has no effect either;
  Flutter does not consult the OS trust store by default.

To see a Flutter app's traffic, either:

- **During development**, point `HttpClient` at sieve explicitly in debug
  builds:

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

  Wrap this so it only runs in debug (`kDebugMode` from
  `package:flutter/foundation.dart`), and remove the
  `badCertificateCallback` override once the sieve CA is trusted, or keep
  it only for debug builds.

- **For a built APK/IPA you can't change**, use `sieve android frida`
  (Android) with a Flutter-aware pinning bypass, or the iOS gadget route
  above; Flutter's own certificate pinning (if the app adds any on top of
  the default BoringSSL verification) needs its own Frida hooks, which the
  bundled `android-unpinning.js` does not cover.
