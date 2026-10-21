# Manual testing guide

How to check sieve by hand, from the laptop up to a real iPhone. Each section ends with what "pass" looks like, mapped to the test items in [PLAN.md](PLAN.md). Do the sections in order: later ones assume the earlier ones work.

Automated tests cover the logic (`cargo test --workspace`). This guide covers what they can't: real devices, real OS proxy settings, and real trust stores.

## 0. Build

```sh
cd ui && npm ci && npm run build && cd ..   # web UI assets, embedded at compile time
cargo build --release
alias sieve="$PWD/target/release/sieve"
```

If you skip the `ui/` build, everything still works, but `http://localhost:8081` shows a "web UI has not been built" page instead of the traffic view.

The CA lives in `~/Library/Application Support/sieve/` (`sieve ca path` prints it). To test against a throwaway CA and config, prefix commands with `SIEVE_CONFIG_DIR=$(mktemp -d)`; devices then need that CA instead.

## 1. Laptop: proxy, CA, and web UI (phase 1 + 3)

Terminal 1:

```sh
sieve start
```

Terminal 2:

```sh
CA="$(sieve ca path | head -1)"
curl -x http://127.0.0.1:8080 --cacert "$CA" https://example.com -o /dev/null -w '%{http_code} %{http_version}\n'
curl -x http://127.0.0.1:8080 --cacert "$CA" --http1.1 https://example.com -o /dev/null -w '%{http_code} %{http_version}\n'
curl -x http://127.0.0.1:8080 --cacert "$CA" -H 'Accept-Encoding: gzip' https://httpbin.org/gzip -o /dev/null
curl -x http://127.0.0.1:8080 http://example.com -o /dev/null      # plain HTTP
sieve ca info                                                      # fingerprint, validity, paths
```

Open http://localhost:8081 and check:

- Each request appears live, with method, host, path, status, size, and duration.
- Clicking a row shows headers, the response body (the gzip one is shown decoded), and timing.
- The text filter, pause, clear, "copy as curl", and HAR download work.

Passthrough: restart with `sieve start --passthrough example.com` and repeat the first curl. It still returns 200, the UI shows a passthrough flow, and `curl -v` shows example.com's real certificate, not one issued by "sieve CA".

**Pass:** both curls return 200 (HTTP/2 and HTTP/1.1), every request is in the terminal log and the UI, and Ctrl+C exits within a second or two. This covers the "curl succeeds and is recorded" item in Phase 1.

### Optional: a browser on the Mac

The easiest way is a separate Chrome window that trusts only the sieve CA. It leaves the Mac's proxy settings, its keychain, and your normal Chrome profile alone. Keep `sieve start` running, then:

```sh
SPKI=$(openssl x509 -in "$(sieve ca path | head -1)" -pubkey -noout \
  | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64)
open -na "Google Chrome" --args \
  --user-data-dir=/tmp/sieve-chrome \
  --proxy-server=127.0.0.1:8080 \
  --ignore-certificate-errors-spki-list="$SPKI" \
  https://example.com
```

The same flags work for Brave (`open -na "Brave Browser" ...`). Chrome shows an "unsupported command-line flag" banner; that is expected. Close the window when you're done; nothing else needs undoing.

Browse a few HTTP/2 sites. There should be no certificate warnings, and the traffic shows up in the UI. Chrome's own background traffic (to `google.com`, `gstatic.com`, and so on) shows up too.

**Safari, or Chrome in your normal profile**, use the Mac's system proxy and keychain instead. Trust the CA in the login keychain, then set the proxy in System Settings > Network > (your service) > Details > Proxies, for both "Web proxy" and "Secure web proxy", pointing at `127.0.0.1:8080`:

```sh
security add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db "$CA"
# when done: turn both proxies off again, then
security delete-certificate -c "sieve CA" ~/Library/Keychains/login.keychain-db
```

## 2. Android mode 1: no root (phase 2)

Uses your existing `Medium_Phone` emulator (a Google Play image is fine here). A USB device with USB debugging enabled works the same way.

```sh
~/Library/Android/sdk/emulator/emulator -avd Medium_Phone &   # wait for it to boot
adb devices                                                    # shows emulator-5554
sieve android setup                                            # add --serial if several devices are attached
```

`setup` sets up `adb reverse`, sets the device's `http_proxy`, pushes the CA to `Download/sieve-ca.crt`, prints the install steps, and then runs the proxy. Leave it running.

On the device, install the CA once: Settings > Security & privacy > More security settings > Encryption & credentials > Install a certificate > CA certificate, then pick `sieve-ca.crt` from Downloads. The menu names vary by Android version; search Settings for "certificate".

Then:

- Open https://example.com in Chrome on the device. Chrome trusts user CAs, so it loads, and the request shows up in the UI with the emulator's client address.
- In another terminal, run `sieve android health`. Every check should pass.
- For your own app, run `sieve android nsc` and add the snippet to a debug build. Its HTTPS traffic (OkHttp, etc.) should then show up. Without the snippet, the app's TLS fails, which shows up as an error flow in the UI.

Press Ctrl+C in the `setup` terminal, then check:

```sh
adb shell settings get global http_proxy    # :0
adb reverse --list                          # empty
```

**Pass:** Chrome and a sample OkHttp debug app are intercepted, and after Ctrl+C the device's internet works normally again. This covers the two Phase 2 test items on the Android side.

## 3. Android mode 2: system CA (phase 4)

Mode 2 needs an image that allows `adb root`. **Google Play images don't**, so `Medium_Phone` won't work. You already have the `android-36 / google_apis` image installed. In Android Studio, open Device Manager, create a device, and pick API 36 **Google APIs** (not "Google Play"). For the plan's test matrix, also make one on API 33 or lower, because Android 13 and below use a different code path.

```sh
~/Library/Android/sdk/emulator/emulator -avd <your Google APIs AVD> &
sieve android inject-ca          # prints "OK: <hash>.0 is in ..." on success
adb shell ls /apex/com.android.conscrypt/cacerts/$(sieve ca hash).0   # Android 14+
adb shell ls /system/etc/security/cacerts/$(sieve ca hash).0          # Android 13 and below
sieve android setup              # routes traffic; you can skip the user-CA install step
```

Open Chrome **and an app with no network security config** (any app you're allowed to test). Both should be intercepted, with no user CA installed. Running `inject-ca` a second time should succeed without stacking mounts. Rebooting the emulator undoes the injection, by design.

**Pass:** on Android 14 and 15 (and on 13 and below if you test it), Chrome and an app without an NSC are intercepted. This covers the Android item in the Phase 4 tests.

## 4. iOS Simulator (phase 2)

> This changes the **whole Mac's** web proxy while it runs. Ctrl+C restores it. If sieve is killed (`kill -9`, crash, or lost power), run `sieve doctor --restore`.

```sh
open -a Simulator                       # boot any iPhone simulator
networksetup -getwebproxy "USB 10/100 LAN"   # note the "before" state (use your service name)
sieve ios-sim setup
```

In the Simulator, open https://example.com in Safari, plus a sample URLSession app if you have one. The traffic shows up in the UI, with no certificate warning. Press Ctrl+C, then:

```sh
networksetup -getwebproxy "USB 10/100 LAN"   # back to the "before" state
sieve doctor                                 # reports nothing left over
```

To test crash recovery, run `sieve ios-sim setup`, `kill -9` it from another terminal, and check that `sieve doctor` reports the leftover state and `sieve doctor --restore` fixes it.

If you'd rather not touch the Mac's proxy at all, see [ios-simulator.md](ios-simulator.md) for setting the proxy in your app's `URLSessionConfiguration` instead.

**Pass:** Safari and a URLSession app in the Simulator are intercepted, and the Mac's proxy settings are back to normal after teardown. This covers the two Phase 2 test items on the iOS side.

## 5. Physical iPhone (phase 4)

The iPhone and the Mac must be on the same Wi-Fi. If macOS asks whether `sieve` may accept incoming connections, allow it.

```sh
sieve ios-device setup
```

It prints a QR code, the setup page URL, and a tokenized UI URL. Open the UI URL on the Mac.

On the iPhone:

1. Scan the QR code with the Camera and open the page in **Safari** (profiles only download from Safari).
2. Tap the profile download. Then go to Settings > General > VPN & Device Management, install "sieve". It shows as "Not Verified", which is normal for an unsigned profile.
3. Go to Settings > General > About > Certificate Trust Settings and turn on full trust for the sieve CA. **This is the step people forget**: without it, the profile is installed but HTTPS still fails.
4. Go to Settings > Wi-Fi > (i) next to your network > Configure Proxy > Automatic, and enter the PAC URL from the setup page (`http://<mac-ip>:8081/proxy.pac`).
5. Open https://example.com in Safari. The setup page's status turns into a green check, and the traffic shows up in the UI.

Undo it: set Configure Proxy back to Off, and remove the profile under VPN & Device Management.

**Pass:** Safari and a sample iOS app are intercepted after scanning the QR code. This covers the iOS item in the Phase 4 tests.

## Reset if something is stuck

```sh
sieve doctor --restore                                  # Mac proxy saved by ios-sim setup
sieve android teardown                                  # or by hand:
adb shell settings put global http_proxy :0
adb reverse --remove-all
networksetup -setwebproxystate "<service>" off          # last resort on the Mac
networksetup -setsecurewebproxystate "<service>" off
sieve ca regenerate                                     # new CA; every device must trust it again
```

## Known blind spots

- **Flutter apps** ignore the system proxy and the OS trust store. You'll see no traffic from them without setting a proxy in Dart code or using Frida (Phase 5).
- **Certificate pinning** shows up as a flow with a TLS error, not as missing traffic.
- **HTTP/3 (QUIC)** goes over UDP and skips the proxy. Most apps fall back to TCP when QUIC fails.
