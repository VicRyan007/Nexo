# Architecture

## Product model

Every Nexo installation is a node. Nodes own an identity, persist authorized community state,
discover peers and can carry media. A headless node is optional, never mandatory.

## Runtime components

1. **Desktop shell**: native Slint interface and platform integration.
2. **Core**: identities, invitations, authorization, events and SFU election.
3. **Network**: libp2p transports, mDNS, Noise, request/response and GossipSub.
4. **Media**: WebRTC sessions, capture, codecs and adaptive topology.
5. **Storage**: SQLite event log, materialized views and content-addressed attachments.
6. **Acceleration**: GPU rendering and hardware media pipelines selected at runtime.

The UI does not access sockets or databases directly. It sends commands to an application
coordinator and receives immutable view updates.

## Connectivity

On a LAN, mDNS discovers nodes and libp2p establishes authenticated encrypted connections. Manual
addresses and invitation codes cover networks where multicast discovery is disabled.

Across the internet, nodes attempt QUIC and TCP, then hole punching. Community relays and TURN are
optional fallbacks. A relay transports encrypted traffic and has no community authority.

## Calls

- 2-4 participants: WebRTC full mesh by default.
- 5+ participants or degraded publisher: elect an eligible participant as SFU.
- Migration: establish the new route in parallel, confirm media, then retire the old route.
- Failure: maintain a ranked standby and re-elect after a bounded heartbeat timeout.

Media is encrypted above the SFU transport so an elected forwarding node cannot decode streams it
is not authorized to consume.

## CPU and GPU allocation

Nexo uses both processors according to workload instead of chasing artificial utilization:

- CPU: network protocol, encryption, packet scheduling, audio processing and control logic.
- GPU: Slint rendering, video composition, scaling and codec acceleration.
- Windows: prefer Direct3D 12 and AMD AMF, with Media Foundation fallback.
- Linux: prefer Vulkan and VA-API through GStreamer, with PipeWire for capture.
- Software codecs remain available when a driver lacks a required hardware profile.

The media capability report used by SFU election includes encoder availability and current GPU
pressure. A node already saturated by gaming is penalized rather than elected merely because its
GPU model is faster.

## Data ownership

Community events are signed, append-only records. Authorized peers replicate the event log and
derive local views in SQLite. Messages remain usable offline and synchronize when peers meet again.
Large attachments are content-addressed, chunked and replicated according to local retention
policy.

## Crate direction

```text
nexo-app -> nexo-net -> nexo-core
         -> nexo-core

future:
nexo-app -> nexo-media -> nexo-core
```

Dependencies must not point back toward the application crate.

## Current media foundation

`nexo-media` owns the call state machine, endpoint discovery, 20 ms microphone framing, pure-Rust
Opus encode/decode, bounded output playback, runtime capability ranking and the native WebRTC peer
adapter. The adapter uses host ICE candidates with no mandatory STUN service and carries Opus over
DTLS/SRTP. A loopback integration test proves ICE, DTLS, SRTP, packet delivery and decoding between
two native peers. CPAL supplies WASAPI capture/playback on Windows and the native Linux backend.
Each remote participant has an independent bounded jitter buffer. It prebuffers three 20 ms RTP
packets, reorders short out-of-order bursts, extends wrapping sequence numbers and rejects stale or
duplicate packets. A confirmed gap is recovered from Opus in-band FEC when the next packet carries
recovery data, with Opus packet-loss concealment as the fallback. Network and playback queues stay
bounded so a slow consumer cannot create unbounded latency. Playout follows a monotonic 20 ms
clock instead of draining network bursts immediately. Concealment is limited to 200 ms during a
stalled stream; after that, playout pauses and cleanly prebuffers a newly arriving sequence.

CPAL stream-error callbacks feed endpoint health into the call engine. Microphone and playback are
independent optional resources: their loss does not tear down libp2p signalling or WebRTC peers.
The failed endpoint is reopened against the current system default with exponential retry from
250 ms to a capped 5 s. Incoming media continues advancing while playback is unavailable, avoiding
stale audio after recovery. Endpoint loss and recovery are surfaced as one-shot UI states.

Signed call presence, offers, answers and leave signals travel over authenticated libp2p sessions.
One offerer per pair is selected deterministically, so the mesh does not need a central call
coordinator. GStreamer remains the planned camera/screen and hardware video pipeline because its
official Rust bindings support Windows and Linux and expose AMF, Media Foundation and VA-API
plugins when installed.
