---
title: Cleanup
description: Restore proxy settings after a session, including after a crash.
---

Teardown commands and Ctrl+C restore everything sieve changed.

If a session crashed, list what was left behind and restore it:

```sh
sieve doctor             # list leftover state
sieve doctor --restore   # restore the Mac proxy and Android http_proxy
```

## Manual reset

If sieve is unavailable, undo the changes by hand.

Android:

```sh
adb shell settings put global http_proxy :0
adb reverse --remove-all   # removes all reverse rules
```

macOS system proxy:

```sh
networksetup -listallnetworkservices
networksetup -setwebproxystate "Wi-Fi" off
networksetup -setsecurewebproxystate "Wi-Fi" off
```
