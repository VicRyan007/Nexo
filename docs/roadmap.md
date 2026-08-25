# Roadmap

## M0 - Foundation

- Rust workspace, CI and contribution policy.
- Persistent device identity.
- Signed network invitations.
- Deterministic SFU election model.

## M1 - LAN social layer

- Native Windows/Linux application.
- mDNS peer discovery and direct encrypted connection.
- Create/join community by code or address.
- Local profiles, channels and replicated text events.

## M2 - P2P voice

- Device selection, mute/deafen and voice activity.
- Two-person and small-group WebRTC mesh.
- Opus, echo cancellation, jitter metrics and reconnect.

## M3 - Video and screen share

- Camera and screen capture through platform backends.
- GPU rendering through Slint's native renderer.
- D3D12/Media Foundation acceleration on Windows and VA-API H.264 acceleration on Linux.
- Runtime capability probing with software fallback.
- Simulcast and adaptive bitrate.

## M4 - Participant-hosted SFU

- Embedded SFU agent.
- Capability measurement and election.
- Seamless mesh-to-SFU and SFU-to-SFU migration.
- Standby host and failure recovery.

## M5 - Internet and hardening

- Optional Kademlia bootstrap discovery, Circuit Relay v2 client reservations, DCUtR and an
  opt-in bounded relay-server mode are implemented; rendezvous and cross-network hole-punch
  validation remain.
- End-to-end media encryption above the SFU.
- Device revocation and founder moderation are implemented; signed updates remain.
- Reproducible Windows and Linux packages.
