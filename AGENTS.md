# sieve

HTTP/HTTPS debug proxy for Android and iOS, written in Rust. The full plan, with phase checklists, is in `docs/PLAN.md`. Tick the checkboxes for items your change completes.

## Layout

- `crates/sieve-core`: CA (`ca/`), MITM proxy (`proxy/`), `Flow` model (`flow.rs`), config and paths (`config.rs`).
- `crates/sieve-device`: `android/` (adb bridge, mode 1, system CA), `ios_sim/`, `ios_device/`.
- `crates/sieve-ui`: axum API, WebSocket feed, embedded frontend.
- `crates/sieve`: the `sieve` CLI. `app.rs` wires the proxy and UI into one session; `cmd/` has one file per subcommand.
- `ui/`: Svelte + Vite frontend. `scripts/android/`: device-side shell scripts.

## Contracts between crates

These public items are used across crates. Change them only additively (new fields, new functions), and never change an existing signature without updating every caller in the same PR:

- `sieve_core::{Flow, FlowSender, flow::channel}`, `Config`, `ProxyConfig`, `UiConfig`, `Paths`
- `sieve_core::CertAuthority::{load_or_create, regenerate, cert_pem, cert_der, export, subject_hash_old}`
- `sieve_core::proxy::run`, `sieve_ui::serve`, `sieve_device::ios_device::routes`
- `sieve_device::android::AndroidBridge` (add methods; don't change `adb` or `shell`)
- `crates/sieve/src/app.rs::run_session`

Add new dependencies to your crate's own `Cargo.toml`. Leave the root `Cargo.toml` alone unless your task says otherwise.

## Verification

Run all three before every commit. All must pass with no warnings:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

If you touch `ui/`, also run `npm ci && npm run build` (and its tests, if any) inside `ui/`.

## Safety rules for tests

- Automated tests must never change host or device state: no real `networksetup`, `adb`, or `simctl` calls. Mock them behind a trait.
- Tests that need certificates generate them into a tempdir; never touch the real config directory.
- Bind test servers to `127.0.0.1` on port 0.

## Commits and PRs

- Conventional commits: `feat(core): ...`, `fix(android): ...`, `test: ...`, `docs: ...`.
- Never add a `Co-Authored-By` trailer or any AI/Claude attribution to commits or PR descriptions.
