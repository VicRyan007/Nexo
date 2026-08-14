# Nexo

Nexo is a local-first, peer-to-peer communication application for Windows and Linux. Every
installation is both a client and a capable network node. A group can communicate on a LAN with
no internet connection, or across the internet using direct connections and optional community
relays.

The project is in its first engineering milestone. The initial vertical slice provides:

- persistent Ed25519 device identity;
- signed, expiring network invitations;
- LAN peer discovery with libp2p mDNS;
- a deterministic SFU election model;
- a native Slint desktop shell;
- acknowledged offline message pagination and signed call signalling;
- native LAN-first WebRTC voice calls using DTLS/SRTP and pure-Rust Opus;
- clocked, bounded RTP jitter buffering with Opus FEC and packet-loss concealment;
- automatic microphone/output recovery without dropping the peer call;
- native microphone capture and bounded output playback.

## Principles

- Native and lightweight: no embedded browser runtime.
- Offline by design: LAN operation must not depend on DNS or a cloud service.
- End-to-end verifiable: identities and control messages are signed.
- Progressive topology: P2P for small calls, participant-hosted SFU for larger calls.
- Open source: AGPL-3.0-or-later.

## Development

Install the stable Rust toolchain, then run:

```powershell
cargo test --workspace
cargo run -p nexo-app
```

Architecture and protocol decisions live in [`docs`](docs/).

## Autonomous continuation

If the primary development session becomes unavailable, preview the durable handoff with:

```powershell
.\scripts\continue-nexo.ps1 -DryRun
```

Run autonomous OpenCode continuation with a bounded number of rounds:

```powershell
.\scripts\continue-nexo.ps1 -Agent OpenCode -MaxRounds 12
```

The script automatically uses Gemini CLI as a fallback when it is installed. Progress and the
completion audit live in `docs/continuation.md`; agent logs stay untracked in `.continuation-logs`.
