# iOS Simulator

`sieve ios-sim setup` gets traffic from booted iOS Simulators into sieve. macOS only.

## How it works

1. Lists booted simulators with `xcrun simctl list devices booted -j`. It stops if none are booted.
2. Installs the sieve CA into each one with `xcrun simctl keychain <udid> add-root-cert <ca.pem>`.
3. Finds the active network service: the default-route interface (`route -n get default`) mapped to a service name through `networksetup -listnetworkserviceorder`.
4. Saves that service's current web and secure web proxy settings (`-getwebproxy`, `-getsecurewebproxy`) to `<config dir>/state/ios-sim-proxy.json`, then sets both to `127.0.0.1:<proxy port>`.
5. Runs the proxy and UI. When the session ends for any reason (Ctrl+C, error, normal exit) the saved settings are restored and the state file is deleted.

The simulator uses the Mac's network stack, so the proxy applies to the **whole Mac** while sieve runs. That is why the settings are saved before anything changes.

If a previous run left a state file behind, `setup` refuses to start (it would otherwise save sieve's own proxy as the "original"). Restore first.

## Restoring

```sh
sieve ios-sim teardown      # restore from the state file
sieve doctor                # report a leftover state file
sieve doctor --restore      # restore from it and delete it
```

If sieve is unavailable or the state file is lost, turn the proxies off by hand (list services with `networksetup -listallnetworkservices`):

```sh
networksetup -setwebproxystate "Wi-Fi" off
networksetup -setsecurewebproxystate "Wi-Fi" off
```

## Without touching the system proxy

If you own the app, point just that app at sieve in a debug build. Install the CA in the simulator first (`sieve ios-sim setup` does it, or run the `simctl keychain` command above).

```swift
let config = URLSessionConfiguration.default
config.connectionProxyDictionary = [
    "HTTPEnable": 1,
    "HTTPProxy": "127.0.0.1",
    "HTTPPort": 8080,
    "HTTPSEnable": 1,
    "HTTPSProxy": "127.0.0.1",
    "HTTPSPort": 8080,
]
let session = URLSession(configuration: config)
```

Wrap this in `#if DEBUG`. Only sessions built from this configuration go through sieve.
