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
- D3D12/AMF acceleration on Windows and Vulkan/VA-API acceleration on Linux.
- Runtime capability probing with software fallback.
- Simulcast and adaptive bitrate.

## M4 - Participant-hosted SFU

- Embedded SFU agent.
- Capability measurement and election.
- Seamless mesh-to-SFU and SFU-to-SFU migration.
- Standby host and failure recovery.

## M5 - Internet and hardening

- DHT/rendezvous discovery, hole punching and optional relay.
- End-to-end media encryption above the SFU.
- Device revocation, moderation and signed updates.
- Reproducible Windows and Linux packages.
