# sieve VPN (Android companion app)

A small Kotlin app with a `VpnService` that captures the device's traffic and forwards TCP to the sieve proxy. Use it when an app ignores the system proxy (raw sockets, Flutter, some SDKs) and mode 1's `http_proxy` does not see its traffic.

Reference: HTTP Toolkit's Android app. This one is a much smaller, simpler design with no dependencies besides the Android SDK.

## How it works

1. The app creates a tun interface routing all IPv4 (and IPv6) traffic into itself.
2. Each TCP connection is terminated locally by a minimal userspace TCP endpoint (`TcpForwarder`). The app sends a `CONNECT <ip>:<port>` to the sieve proxy and relays the bytes over that tunnel.
3. sieve sniffs the stream. TLS is intercepted using the SNI from the ClientHello; plain HTTP is parsed as usual. The result shows up in the UI like any other flow.
4. UDP is relayed untouched (so DNS works) over sockets that bypass the VPN. UDP port 443 (QUIC) is dropped so apps fall back to HTTPS over TCP. IPv6 packets are dropped so apps fall back to IPv4.

The proxy is reached at `127.0.0.1:8730` on the device through `adb reverse`, or at any host and port you type into the app (sieve must then listen on an address the phone can reach).

## Build

Needs JDK 17 or 21 and the Android SDK (platform 35). Android Studio works; from the command line:

```sh
cd android-vpn
echo "sdk.dir=$ANDROID_HOME" > local.properties
./gradlew testDebugUnitTest assembleDebug
```

The APK is `app/build/outputs/apk/debug/app-debug.apk`. Install it with `adb install -r app/build/outputs/apk/debug/app-debug.apk`.

The unit tests (packet encoding, the TCP state machine against a fake proxy connection) run on the JVM and touch no device.

## Use

```sh
sieve android vpn
```

This runs `adb reverse`, pushes the CA to the device, opens the app pointed at the proxy, and starts the proxy and UI. Then:

1. Accept Android's VPN consent dialog (first time only). The app starts the VPN once you do.
2. Install the CA if you have not (the command prints the steps). Apps that target Android 7+ still need to trust user CAs: use `sieve android nsc` in debug builds, or `sieve android inject-ca` on an emulator or rooted device.
3. Stop with Disconnect in the app. When `sieve android vpn` exits it removes the `adb reverse`.

Without the CLI, open the app, enter the proxy address, and tap Connect.

## Limits

- Not tested on a real device or emulator yet; only the JVM unit tests have run.
- The TCP endpoint has no retransmission timer, congestion control or window scaling, and no backpressure from a slow app to the proxy connection (data from the app is queued in memory). Fine for debugging traffic; not a general VPN.
- Connections are tunnelled by IP, not hostname. TLS is matched by SNI, but plain HTTP to a host without a `Host` header and TLS without SNI show the IP.
- DNS is sent to 8.8.8.8 regardless of the device's DNS settings. Private DNS (DoT) goes through the tunnel as an ordinary TCP flow.
- No foreground notification; Android keeps the service alive as the active VPN. A vendor battery manager may still kill it.
- The app excludes itself from the VPN, and only one VPN can be active at a time.
