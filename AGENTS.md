# Engineering Guide

Nexo is a native Rust application. Keep the UI, networking and domain rules separated.

## Boundaries

- `nexo-core`: deterministic domain logic. No sockets, UI or platform APIs.
- `nexo-net`: libp2p discovery, transport and protocol adapters.
- `nexo-app`: Slint UI and application orchestration.
- `nexo-media`: native capture/playback, codecs, WebRTC and call topology.
- `nexo-video`: camera/screen capture and hardware capability probing. Its Windows backend is the
  documented, isolated `unsafe` exception (see `crates/nexo-video/src/lib.rs`).

## Rules

- Do not add Electron, WebView or browser-runtime dependencies.
- Do not introduce a mandatory cloud service.
- Network inputs are untrusted and must be bounded and validated.
- Avoid `unsafe`; exceptions require a written design decision and isolated wrapper.
- Tests are required for invitation validation, election changes and protocol parsing.
- Preserve Windows and Linux support in every platform-specific change.

## Verification

Run `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and
`cargo test --workspace` before integrating a change.

For autonomous handoff, read `docs/continuation.md` first and keep its checkpoint current. The
PowerShell entry point is `scripts/continue-nexo.ps1`.
