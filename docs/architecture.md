# Architecture

## Product model

Every Nexo installation is a node. Nodes own an identity, persist authorized community state,
discover peers and can carry media. A headless node is optional, never mandatory.

## Runtime components

1. **Desktop shell**: native Slint interface and platform integration.
2. **Core**: identities, invitations, authorization, events and SFU election.
3. **Network**: libp2p transports, mDNS, Noise, signed request/response and local synchronization. Sync protocol `0.4.0` replicates community channel metadata, encrypted community messages and signed membership commits.
4. **Media**: WebRTC sessions, capture, codecs and adaptive topology.
5. **Storage**: SQLite event log, materialized views and chunked, hash-verified attachments.
6. **Acceleration**: GPU rendering and hardware media pipelines selected at runtime.

The UI does not access sockets or databases directly. It sends commands to an application
coordinator and receives immutable view updates.

## Connectivity

On a LAN, mDNS discovers nodes and libp2p establishes authenticated encrypted connections. Discovered
and manually invited addresses are retained with bounded memory (256 peers, eight addresses and
eight concurrent dials) and redialed with exponential backoff when startup races or a peer restart
make the first attempt fail. Manual addresses and invitation codes cover networks where multicast
discovery is disabled.

The current release is intentionally LAN-first: it uses direct TCP/QUIC connections discovered by
mDNS or supplied through an invitation/address. An optional Kademlia behaviour can be bootstrapped
with authenticated `/p2p/<PeerId>` multiaddrs from `NEXO_KAD_BOOTSTRAP`; it feeds learned addresses
back into the same bounded dialer. No DHT node, internet connection or relay is required for LAN
operation. Optional WebRTC STUN/TURN servers can be configured with `NEXO_STUN_SERVERS` and
`NEXO_TURN_SERVERS`. Optional libp2p Circuit Relay v2 clients and DCUtR can be configured with
`NEXO_RELAY_SERVERS`; the client reserves `/p2p-circuit` addresses and keeps direct TCP/QUIC as
the preferred path when available. Any installation can also opt into the bounded relay-server
behaviour with `NEXO_RELAY_SERVER=1` and `NEXO_RELAY_LISTEN_PORT`; it remains disabled by default.
For a relay outside the LAN, `NEXO_RELAY_PUBLIC_ADDRESS` supplies the advertised public listener;
port forwarding and distributing the authenticated relay multiaddr remain operator concerns.
The optional `NEXO_DISABLE_MDNS=1` switch disables only automatic LAN discovery, which is useful
for isolating invite and relay paths in tests. A reserved address already ends in
`/p2p-circuit/p2p/<local-peer>` and can be dialed by another authenticated participant.

## Calls

- 2-4 participants: WebRTC full mesh by default.
- 5+ participants or degraded publisher: elect an eligible participant as SFU.
- Migration: elect a replacement and switch eligible media targets while existing signaling
  connections remain alive; cross-machine media confirmation is still a validation item.
- Failure: maintain a ranked standby and re-elect after a bounded heartbeat timeout.

Media is encrypted above the SFU transport so an elected forwarding node cannot decode streams it
is not authorized to consume. Frames use ChaCha20-Poly1305 with a fresh random nonce and an
authenticated sequence header; the call engine shares one sender sequence across audio and video
so the nonce policy remains safe when both codecs are active. The frame key is derived from the
current community epoch; removal commits deliver the next epoch secret only through per-member
X25519 envelopes, and active calls rekey only when a membership hash actually changes.

## CPU and GPU allocation

Nexo uses both processors according to workload instead of chasing artificial utilization:

- CPU: network protocol, encryption, packet scheduling, audio processing and control logic.
- GPU: Slint rendering, video composition, scaling and codec acceleration.
- Windows: use native Windows Graphics Capture/DXGI and Media Foundation capability probing;
  synchronous and event-driven asynchronous hardware MFT encoders are eligible when they pass
  initialization, with VP8 as the fallback.
- Linux: use V4L2 for cameras and XDG Portal/PipeWire for screens. H.264 encoding uses the
  runtime-loaded `moq-vaapi` VA-API path when a render node and compatible H.264 profile initialize;
  otherwise the call remains on software VP8 without failing startup.
- Software codecs remain available when a driver lacks a required hardware profile. Capability
  advertisement calls the same encoder constructor used by the media engine, so a present
  `/dev/dri` node or libva library alone cannot select a broken path.

The media capability report used by SFU election includes encoder availability, measured CPU
headroom and a conservative GPU capability hint. The application republishes that report every
two seconds during an active call alongside authenticated heartbeats, so changing local
conditions can participate in a new election. Failover checks the standby heartbeat as well and
never promotes an expired standby. Vendor-specific GPU pressure telemetry remains future work,
so a GPU model alone is never treated as proof that a node is available.

Connected peers expose their lowest REMB estimate to the call engine. The engine selects a bounded
360p/15 FPS, 480p/24 FPS or 720p/30 FPS profile and recreates the active software or hardware
encoder only when that tier changes; a failed hardware reconfiguration keeps the last working
profile and software VP8 remains the fallback. A transient H.264 device reset uses bounded
backoff to recreate the encoder instead of leaving an already-negotiated H.264 track permanently
silent.

## Data ownership

Community events are signed, append-only records. Authorized peers replicate the event log and
derive local views in SQLite. Community messages are stored and synchronized as authenticated
ChaCha20-Poly1305 envelopes; authorized peers decrypt them locally when they meet again. A
revocation commit removes the member from future history sharing, while the current membership
state rebuilds additive joins deterministically so concurrent invitations converge.
Large attachments are chunked and SHA-256 verified before they are accepted locally; replication
and content-addressed retention are future storage work.

## Crate direction

```text
nexo-app -> nexo-net -> nexo-core
         -> nexo-media -> nexo-video
                      -> nexo-core
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
stale audio after recovery. Requested camera devices use the same bounded retry policy, while a
failed screen capture clears sharing and reports the existing video-unavailable state. Endpoint
loss and recovery are surfaced as one-shot UI states. The application contains a panic boundary
around each media tick and attempts to close the engine before returning the call UI to idle;
native H.264 probing is likewise isolated behind a panic-safe software fallback.

Signed call presence, offers, answers and leave signals travel over authenticated libp2p sessions.
One offerer per pair is selected deterministically, so the mesh does not need a central call
coordinator. ICE/DTLS state transitions are surfaced as connected, disconnected, failed or closed;
the failure text distinguishes a network/NAT path problem from a local media-device failure. The
current native capture paths are intentionally smaller than a GStreamer
dependency: Windows uses Media Foundation and Windows Graphics Capture, while Linux uses V4L2 and
XDG Portal/PipeWire. Windows asynchronous H.264 transforms run on a dedicated Media Foundation
worker that consumes `METransformNeedInput` and `METransformHaveOutput` events; VP8 remains the
fallback when native initialization or runtime encoding is unavailable.

Once the call is connected, the offerer-created `nexo-control` SCTP DataChannel is retained by
both peers. It carries bounded binary messages with a 4 MiB sender buffer and a bounded receiver
queue; async backpressure prevents a large attachment from silently dropping chunks. The app
serializes the existing signed file offer and 8 KiB chunks over this channel when the whole
authorized audience is present, and falls back to libp2p file transfer when a member is outside
the call.
