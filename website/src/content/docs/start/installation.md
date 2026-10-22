---
title: Installation
description: Install sieve from a prebuilt binary or build it from source.
---

## Prebuilt binary

Download the archive for your platform from [Releases](https://github.com/saamodra/sieve/releases), then:

```sh
tar xzf sieve-*.tar.gz
sudo mv sieve-*/sieve /usr/local/bin/
```

On macOS, if a browser download gets blocked by Gatekeeper, remove the quarantine flag:

```sh
xattr -d com.apple.quarantine /usr/local/bin/sieve
```

## From source

Requirements: Rust 1.90+ and Node.js (for the web UI).

```sh
git clone https://github.com/saamodra/sieve.git
cd sieve
(cd ui && npm ci && npm run build)   # build the UI first so it gets embedded
cargo install --path crates/sieve
```

This installs `sieve` into `~/.cargo/bin`, which rustup adds to your `PATH`. Check it with:

```sh
sieve --help
```

To update, pull the latest changes and run both build commands again.

:::note[Pre-alpha]
Features are implemented and covered by automated tests, but device setup has not yet been verified end to end on real hardware.
:::
