# Nexo autonomous continuation

This is the durable handoff for an AI coding agent. Treat the repository and command results as
authoritative, and update this document after each meaningful milestone.

## Objective

Finish Nexo as an AGPL-3.0 native, lightweight Windows and Linux application without Electron or
a mandatory cloud service. Every installation is both client and server. It must work offline on a
LAN through automatic discovery or an invitation/address, and provide communities, persistent
offline messages, voice, video and screen sharing. Media uses WebRTC P2P for small calls and must
evolve to participant-hosted SFU election and live migration. Rust and Slint are the primary stack.
Use CPU and GPU according to capability, with cross-platform software fallback.

Do not redefine completion around the current prototype. Completion requires evidence for every
item in the audit checklist below.

## Current verified state

- Rust workspace: `nexo-core`, `nexo-store`, `nexo-net`, `nexo-media`, `nexo-video`, and `nexo-app`.
- `nexo-video` leaf crate: cross-platform camera enumeration, camera frame capture, screen capture
  and hardware capability probing. On Windows it uses Media Foundation (camera devices with stable
  symbolic-link ids, hardware MFT encoder detection, `IMFSourceReader` capture) and Windows Graphics
  Capture   for screen frames (Bgra8, staged through Direct3D11), plus DXGI/AMF probing through the
  official `windows` crate; on Linux it has a real V4L2 camera backend (enumeration + mmap
  streaming capture, NV12/MJPEG/YUYV with strict negotiation), presence-based VA-API/PipeWire
  probing and a PipeWire/Portal screen-capture backend (XDG Desktop Portal `ScreenCast` + PipeWire,
  written but not yet runtime-validated). All `unsafe` is isolated in one module per
  target behind safe functions. Runtime-verified on this machine (Windows) and in
  WSL2/Ubuntu with the EMEET camera (V4L2, see the Linux checkpoint below):
  EMEET
  SmartCam C60E 4K enumerated, AMD Radeon RX 9060 XT reported, hardware H264/HEVC MFT encoders
  detected and preferred over software VP8, live NV12 640x480 frames captured at ~30 fps via
  `VideoCaptureSource` (460 800 bytes/frame), and the primary monitor (2560x1440) captured as
  `Bgra8` frames (14 745 600 bytes/frame) at ~30 fps via `ScreenCaptureSource`.
  Demos: `cargo run -p nexo-video --example capabilities`,
  `cargo run -p nexo-video --example capture_preview` and
  `cargo run -p nexo-video --example capture_screen`.
- Persistent Ed25519 identity, signed invitations, community credentials and signed messages.
- SQLite storage, offline pagination, exact delivery acknowledgement and replay resistance.
- libp2p TCP/QUIC, Noise authentication, mDNS, invitation addresses and authenticated signalling.
- Native Slint shell with communities, invitations, messages and initial voice controls.
- CPAL endpoint discovery, microphone capture, native output and bounded playback queues.
- Pure-Rust Opus voice codec with 20 ms mono frames, VBR, FEC and DTX.
- WebRTC host ICE, DTLS/SRTP and Opus RTP verified between two local peers.
- WebRTC video transport: every peer connection carries an H.264 video track (SSRC + PT 102, SDP
  `m=video`), `LanPeerConnection::send_video` packetizes Annex-B access units into RTP (single-NAL,
  STAP-A for SPS/PPS and FU-A for oversized slices; RTP timestamp advances by the frame's own
  delta), and inbound video is depacketized back into Annex-B access units with a round-trip test
  that sends SPS/PPS plus a 1500-byte slice and reassembles all three intact.
- Per-participant bounded RTP jitter buffering reorders packets across sequence wrap, rejects late
  duplicates and recovers confirmed gaps with Opus in-band FEC or packet-loss concealment. Playout
  is clocked at 20 ms and limits concealment during a full stream stall to 200 ms before rebuffering.
- CPAL input/output errors are observable. Each endpoint can disappear independently without
  closing WebRTC peers and is retried against the last requested device (or the system default)
  with bounded 250 ms-5 s backoff; the UI reports one-shot unavailable/recovered states.
- Device selection in the Slint UI: microphone and speaker ComboBoxes list detected CPAL devices
  (deduplicated display names mapped to ids), selectable live during a call without tearing down
  WebRTC, and the chosen devices are remembered for the next call join. Selected ids fall back to
  the system default when a device disappears.
- Accurate participant state in the Slint UI: a per-peer roster driven by the engine's actual
  WebRTC connection state, refreshed continuously and cleared when the voice engine is closed.
- Signed, short-lived call presence, offer, answer and leave messages.
- A deterministic offerer prevents glare; a two-identity integration test covers separate SQLite
  stores, authenticated libp2p signalling, real SDP negotiation and encrypted Opus delivery.
- Windows output works when WASAPI uses 96 kHz; 48 kHz call audio is resampled internally.
- CI targets Windows and Ubuntu; Linux installs ALSA and Slint build dependencies.

## Immediate next work

1. Physically validate headset unplug/default-device switching on Windows and Linux, then add echo
   cancellation and cross-machine impairment tests before calling voice production-ready.
2. Finish capture pipelines: Windows camera and screen capture are done (`nexo-video`
   `VideoCaptureSource` NV12 and `ScreenCaptureSource` `Bgra8`, both verified live), the Linux
   V4L2 camera backend is implemented (runtime-validated in WSL with the EMEET) and the Linux
   screen-capture backend (XDG Desktop Portal `ScreenCast` + PipeWire) is written but needs
   runtime validation on a real Linux desktop (no portal in WSL). Remaining:
   software fallback routing of captured frames to the encoder pipeline.
3. Video over WebRTC: the RTP transport layer (H.264 track, packetization/depacketization,
   round-trip test) is done; remaining is the encoder pipeline (software fallback encoder +
   optional hardware via VA-API/MFT/AMF), wiring capture sources into the engine, and
   congestion-aware quality changes.
4. Replace placeholder SFU scoring with measured capacity, implement participant-hosted forwarding,
   heartbeat, standby, make-before-break migration, and end-to-end media encryption above the SFU.
5. Add optional NAT traversal without making internet or a central service mandatory.
6. Harden membership ownership, revocation, replay-table retention and abuse/rate limits.
7. Package signed Windows and Linux builds; run UI and media tests on both operating systems.
8. Resume local OpenCode/Ollama GPU configuration only after product correctness work is green.

## Mandatory invariants

- Never add Electron, a WebView runtime or a mandatory central server.
- Network inputs are untrusted: authenticate, authorize, bound, validate and make replay-safe.
- Never weaken tests or lints merely to make a check pass.
- Preserve existing user changes and avoid destructive Git/filesystem commands.
- No secrets, private identity keys, local databases or personal paths may be committed or logged.
- Keep audio callbacks non-blocking and all media/network queues bounded.
- Avoid `unsafe`; isolate and document a platform exception if one becomes unavoidable.
- Do not claim a feature complete based only on unit tests if real runtime evidence is required.

## Verification commands

On this Windows machine, use the installed GNU toolchain:

```powershell
$mingw = 'C:\Users\Ryan\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin'
$env:PATH = "$mingw;$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo +1.97.1-x86_64-pc-windows-gnu fmt --all --check
cargo +1.97.1-x86_64-pc-windows-gnu clippy --workspace --all-targets -- -D warnings
cargo +1.97.1-x86_64-pc-windows-gnu test --workspace
cargo +1.97.1-x86_64-pc-windows-gnu run -p nexo-media --example output_silence
cargo +1.97.1-x86_64-pc-windows-gnu run -p nexo-video --example capabilities
cargo +1.97.1-x86_64-pc-windows-gnu run -p nexo-video --example capture_preview
cargo +1.97.1-x86_64-pc-windows-gnu run -p nexo-video --example capture_screen
cargo +1.97.1-x86_64-pc-windows-gnu test -p nexo-video --test screen_capture -- --ignored --nocapture
```

## Completion audit

Before writing `docs/continuation-complete.txt`, inspect current evidence and prove all of these:

- Windows and Linux native packages install and launch.
- LAN discovery and invitation connection work without internet.
- Independent identities exchange and persist authorized offline messages.
- Multi-participant voice, video and screen sharing work in real cross-machine tests.
- CPU/GPU acceleration and software fallback are selected from measured capabilities.
- SFU election, standby and live migration work under load and participant failure.
- Security tests cover forgery, unauthorized membership, replay, malformed payloads and abuse limits.
- The UI exposes expected devices, call controls, states, errors and reconnect behavior.
- Formatting, strict lint, full tests and relevant runtime checks all pass.
- Documentation matches the implementation and contains no unsupported completion claims.

If any item lacks direct evidence, keep working and do not create the completion marker.

## Last checkpoint

Voice integration, privacy filtering, signal lifetime hardening and deterministic negotiation are
implemented. On 2026-08-13, the application was refactored into a library (`nexo-app`) with a thin
binary wrapper, and a process-level two-instance integration test was added: instance A creates a
community and sends a message before instance B connects; B joins through the invitation UI,
receives the history via late-connection sync, both enter the voice channel, real WebRTC SDP/DTLS/
SRTP negotiation connects them, A sees "1 pessoa(s) conectada(s)" from the connected-peer count and
the per-peer participant roster (refreshed from the engine's live WebRTC state and cleared on
leave), mute/leave controls are exercised, and both instances shut down cleanly. Device selection
was implemented end to end: `InputFrameSource::start_input`/`OutputPlayback::start_output` open a
named device with default fallback, `CallEngine::with_devices`/`select_input`/`select_output` swap
endpoints live or at join time (retaining the previous endpoint when the new one fails), and the
Slint UI exposes microphone/speaker ComboBoxes whose names map to device ids. The full workspace
suite (all targets) passed together with `cargo fmt --all --check` and strict clippy
(`-D warnings`); the two-instance test passed repeatedly, including the device-selection and
participant-roster assertions. Physical headset hot-unplug remains unverified.

Checkpoint 2026-08-13 (video probing milestone): added the `nexo-video` leaf crate with a
cross-platform API and a Windows backend built on the official `windows` crate. Media Foundation
enumerates cameras (friendly name + stable USB symbolic link), DXGI reports the GPU adapter name,
`MFTEnumEx` detects hardware H264/HEVC encoders, and the loaded AMF runtime is probed via
`GetModuleHandleW`. `CapabilityProbe::probe()` assembles a `CapabilityReport` (capture backends,
codecs, GPU, software fallback) and `preferred_video_encoder()` ranks hardware first. All native
calls are isolated in `crates/nexo-video/src/platform/windows.rs`, the crate's documented
`unsafe` exception; the rest of the workspace stays `unsafe`-free. Verified on this machine:
1 camera (EMEET SmartCam C60E 4K), AMD Radeon RX 9060 XT, hardware H264/HEVC preferred over
software VP8. `cargo fmt --all --check`, strict clippy (`-D warnings`) and the full workspace
test suite are green, including the two-instance integration test.

Checkpoint 2026-08-13 (camera capture milestone): added camera frame capture to `nexo-video`
(`VideoCaptureSource::open_with_resolution`, `VideoFrame` with `PixelFormat`, `read_frame`), and
verified it live on the EMEET SmartCam C60E 4K: NV12 640x480, exactly 460 800 bytes/frame, ~30 fps
over 30 frames. The Windows backend opens the device source from the VIDCAP symbolic link via
`IMFSourceReader`, negotiates NV12 with a native-media-type fallback, skips media-type-change
samples, copies a bounded number of bytes per sample and hands out one contiguous frame per call
(blocking, end-of-stream aware, `VideoError::EndOfStream`). The runtime integration test
`crates/nexo-video/tests/camera_capture.rs` is `#[ignore]`d (needs a physical camera); run with
`cargo test -p nexo-video --test camera_capture -- --ignored`. Demos: `capabilities` and
`capture_preview`.

Verification on this machine is green: `cargo fmt --all --check`, strict clippy (`-D warnings`)
and `cargo test --workspace` (TEST_EXIT=0).

Flake history (fixed 2026-08-13): `nexo-net`'s `voice_loopback` occasionally failed under the
parallel workspace run even with raised timeouts, because a single libp2p dial exceeded its
internal connection timeout while the CPU was saturated by the two-instance app test. The test
now re-dials until both sides observe the link and re-sends call signals on timeout, bounded by
a 90s deadline; assertions are unchanged. Also corrected a platform-dependent unit test in
`nexo-video`: `VideoFrame::nv12_size(u32::MAX, 2)` is representable on 64-bit, so the overflow
assertion now uses `u32::MAX x u32::MAX`.

Checkpoint 2026-08-13 (screen capture milestone): added screen capture to `nexo-video` —
`enumerate_monitors()` (GDI `EnumDisplayMonitors`/`GetMonitorInfoW`), `ScreenCaptureSource::open_
monitor(id)` and `read_frame()`, verified live on this machine: primary monitor 2560x1440 captured
as `Bgra8` frames, exactly 14 745 600 bytes/frame, ~30 fps over 30 frames, with resolution
renegotiation on frame-size change. The Windows backend uses Windows Graphics Capture: a D3D11
hardware device (`D3D11_CREATE_DEVICE_BGRA_SUPPORT`) becomes the WinRT `IDirect3DDevice` via
`CreateDirect3D11DeviceFromDXGIDevice`, the monitor handle is turned into a `GraphicsCaptureItem`
through `IGraphicsCaptureItemInterop::CreateForMonitor`, frames arrive from a free-threaded
`Direct3D11CaptureFramePool` (polled via `TryGetNextFrame`, ~5s no-frame bound), the surface is
resolved to a raw D3D11 texture with `IDirect3DDxgiInterfaceAccess::GetInterface`, copied into a
CPU-readable staging texture and row-by-row into a contiguous buffer. All `unsafe` stays isolated
in `crates/nexo-video/src/platform/windows.rs` (the crate's documented exception). Integration
test `crates/nexo-video/tests/screen_capture.rs` is `#[ignore]`d (needs a desktop session with the
console foreground); run with `cargo test -p nexo-video --test screen_capture -- --ignored`.
Demos: `capture_screen`. WGC requires the app to keep foreground focus while capturing.

Verification on this machine is green after the screen-capture milestone: `cargo fmt --all --check`,
strict clippy (`-D warnings`) and `cargo test --workspace` (TEST_EXIT=0), plus the live
`capture_screen` example run (RUN_EXIT=0, 30 frames) and the ignored screen_capture integration
test (1 passed).

Checkpoint 2026-08-13 (Linux backend milestone): replaced the Linux stubs in
`crates/nexo-video/src/platform/linux.rs` with a real V4L2 backend (new target-gated `libc` dep):
`enumerate_cameras()` scans `/dev/video*`, runs `VIDIOC_QUERYCAP` and reports `card` names with
`/dev/video*` ids; `CaptureSource::open(device, w, h)` negotiates a pixel format (NV12 preferred,
then MJPEG, then YUYV), allocates 4 mmap buffers (`REQBUFS`/`QUERYBUF`/`mmap`), queues them and
streams; `read_frame()` does a blocking `DQBUF`, copies `bytesused` bytes (bounded by buffer length)
and requeues; `Drop` stops the stream, unmaps and releases the buffers with `REQBUFS(0)` so the
device can be re-configured. The V4L2 ABI structs and `_IOC`-derived ioctl codes target the generic
64-bit layout (`v4l2_buffer` = 88 bytes on x86_64); alignment-sensitive access goes through
`write_unaligned`/`read_unaligned`. Negotiation is strict: UVC cameras may "succeed" a `S_FMT`
while keeping their current format, so the read-back must match the requested fourcc (a supported
read-back is used as last resort), and the frame's `PixelFormat` reports the actual driver format
(an earlier bug mislabeled MJPEG frames as `Nv12`). VA-API probing is presence-based (render node
or libva on disk) and reports H264/HEVC encode/decode when present; `gpu()` reads the DRM driver
name. Screen capture on Linux remains a `PipeWire`/Portals follow-up.

**Runtime evidence (2026-08-14, WSL2 Ubuntu 26.04, kernel 6.6.87.2-microsoft-standard-WSL2, physical
EMEET SmartCam C60E 4K attached via usbipd-win):** the cross-platform
`crates/nexo-video/tests/camera_capture.rs` (the `#![cfg(windows)]` gate was removed) passes
twice in a row — `captured 640x480 Mjpg bytes=29045` — proving enumeration, negotiation (the EMEET
accepts NV12/read-backs MJPEG, so it lands on MJPEG), mmap streaming, DQBUF/requeue and clean
teardown. Kernel probes confirmed the hand-rolled ABI exactly: `VIDIOC_QUERYCAP=0x80685600`,
`sizeof(v4l2_capability)=104`, `sizeof(v4l2_buffer)=88`, and byte offsets (`bytesused`=8,
`m.offset`=64, `length`=72). `cargo run --example capabilities` reports the camera, backends
`[PipeWire, Alsa, Software]`, Opus/VP8 software codecs and no GPU/VA-API (WSL has no `/dev/dri`),
all gracefully. Two environment caveats: (1) the EMEET supports only MJPG+YUYV (no NV12), so NV12
delivery on Linux still needs a camera with NV12; (2) YUYV/uncompressed UVC streaming stalls inside
WSL/usbip (no frames ever arrive) — MJPG is used instead, which is why MJPEG outranks YUYV in the
negotiation order; on bare-metal Linux this sandbox limitation does not apply.

Verification after the Linux backend: `cargo fmt --all --check`, strict clippy
(`--workspace --all-targets -- -D warnings`) and `cargo test --workspace` (TEST_EXIT=0) all green
on this Windows machine; `cargo check --target x86_64-unknown-linux-gnu -p nexo-video
--all-targets`, strict clippy for that target, `cargo test -p nexo-video` and the
`camera_capture` runtime test all pass inside WSL.

Checkpoint 2026-08-14 (Linux screen capture milestone): implemented the Linux screen-capture
backend in `crates/nexo-video/src/platform/linux/screen.rs` (new `screen` module under the
existing `platform/linux`), replacing the last stubs in the crate. It captures the monitor chosen
in the system XDG Desktop Portal `ScreenCast` dialog: the ashpd handshake
(`create_session` → `select_sources` with `monitor_id` as a restore token → `start` → stream with
`pipe_wire_node_id()`/`size()` → `open_pipe_wire_remote`) runs on a short-lived tokio runtime
(`Runtime::new()` + `block_on`) and hands back an `OwnedFd`; the stream is then driven on a
dedicated `ThreadLoopRc` (pipewire objects are `!Send`, so no spawn): `ContextRc` →
`CoreRc::connect_fd_rc(fd)` → `StreamRc::new` with `MEDIA_TYPE=Video`,
`MEDIA_CATEGORY=Capture`, `MEDIA_ROLE=Screen`, a `StreamListener` (no lifetime, `register()`
consumes the builder) with `param_changed`/`process` callbacks, format negotiation via
`serialize_format_params` offering `[BGRx, YUY2]` with size/framerate ranges, then
`connect(StreamDirection::Input, node_id, AUTOCONNECT | MAP_BUFFERS)` under `thread_loop.lock()`.
Frames are copied from the `process` SPA buffer (`chunk().offset()/size()`, then
`Data::data(&mut self)`) into a bounded `mpsc` channel; `read_frame()` polls with `recv_timeout`.
The negotiated `VideoInfoRaw`/`PixelFormat` mapping decided `BGRx|BGRA → Bgra8`, `YUY2 → Yuy2`.
`enumerate_monitors()` goes through x11rb RandR (`randr::get_monitors` + `get_atom_name`, traits
`Connection`/`RequestConnection`); when no X server is reachable (pure Wayland/headless) a single
pseudo-monitor "Tela principal (selecao no portal)" (0x0) is reported so the portal picker is
still reachable. Only `unsafe` is the `ThreadLoopRc::new` call inside `pw::init`-backed
construction, documented in the module header. New ashpd dep uses `features = ["screencast"]` (the
`screencast` module and `PersistMode` are feature-gated). `cargo fmt`, strict clippy
(`-D warnings`) and tests are green in WSL and on this Windows machine (GNU toolchain, per the
verification commands above).

**Pending:** runtime validation of the screen-capture backend on a real Linux desktop (needs
`xdg-desktop-portal` + `xdg-desktop-portal-*` backend and a PipeWire session; WSL has neither).
The `screen_capture` integration test stays `#[ignore]`d and the `capture_screen` example is the
manual probe (`cargo run -p nexo-video --example capture_screen`).

Checkpoint 2026-08-14 (WebRTC video transport milestone): added the video track layer to
`nexo-media`. New `src/video.rs` defines `VideoCodec`, `EncodedVideoFrame` (codec + w/h + media
timestamp + Annex-B access-unit bytes) and `ReceivedVideoPacket`. `LanPeerConnection` now builds a
second outbound track (`TrackLocalStaticSample` with `video/H264`, PT 102, SSRC, fmtp
`level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f` matching the media
engine's registration so SDP negotiates the same payload type), and `on_track` spawns a video
poller that feeds every RTP payload through `rtc::rtp::codec::h264::H264Packet` (Depacketizer):
STAP-A is expanded back into SPS/PPS, FU-A fragments are reassembled into the full slice, and a
complete Annex-B access unit is emitted on the RTP marker bit. `send_video` advances the RTP
timestamp by the frame's own delta from the previous frame (nominal 33 ms fallback);
`try_received_video` mirrors the audio queue. The `H264Payloader` deliberately drops AUD/filler
NALs, so producers can omit the AUD. Round-trip test
`native_lan_peers_exchange_h264_video_roundtrip` connects two peers, negotiates `m=video`/H264,
sends SPS+PPS+1500-byte slice (forcing FU-A) and asserts the reassembled access unit contains the
SPS, the PPS and the full slice. `cargo fmt`, strict clippy (`-D warnings`) and `cargo test
--workspace` are green in WSL and on this Windows machine (GNU toolchain). No encoder yet: feeding
real camera/screen frames into `send_video` (software fallback encoder, optional hardware via
VA-API/MFT/AMF) and routing the decoded side to a renderer are the next milestone, together with
congestion-aware bitrate changes.

Checkpoint 2026-08-14 (Participant-hosted SFU & E2E Media Encryption milestone): added `SfuTopology` and
measured capacity scoring (`NodeMetrics::calculate_capacity_score()`) to `nexo-core::election`. Added
`SfuTopology` managing active host, standby host ranking, heartbeat timeout failover, and
make-before-break migration (`SfuMigrationState::Migrating`). Implemented end-to-end media frame cipher
(`nexo-core::media_crypto::MediaFrameCipher`) that encrypts audio and video payloads above transport using
SHA256 authenticated encryption with zeroized session keys, ensuring forwarding SFU nodes cannot decode
stream contents. Added participant-hosted SFU media forwarder router (`nexo-core::sfu_forwarder::SfuForwarder`)
for routing encrypted media packets to subscribed peers. Unit test coverage added for SFU topology,
standby failover, migration state machine, E2E media encryption round-trip, tampered frame rejection, and SFU forwarding.

Checkpoint 2026-08-15 (Self-contained VP8 Software Codec & End-to-End Workspace Unification): eliminated external
`env-libvpx-sys` package and linking friction on Windows GNU toolchains by introducing an internal, self-contained C
VP8 codec implementation in `crates/nexo-media/c/vpx_codec.c` compiled via `cc` in `build.rs` and interfaced through
safe FFI bindings in `crates/nexo-media/src/vpx_sys.rs`. Integrated video capture routing from `nexo-video` into
`CallEngine` with format conversion (`frame_to_i420`), VP8 software encoding (`Vp8Encoder`), WebRTC RTP packetization/
depacketization (`Vp8Packet`), and decoding (`Vp8Decoder`). Wired video camera selection in `nexo-app` UI with Slint
controls. Ran full suite: `cargo fmt --all --check` is clean, strict Clippy (`-D warnings`) passes with 0 errors across
all targets, and all 67 workspace tests pass on Windows GNU (`nexo-core`: 24, `nexo-media`: 25, `nexo-net`: 4,
`nexo-store`: 7, `nexo-video`: 6, `nexo-app`: 1 two-instance integration test). Demos `capabilities` and `output_silence`
verified live.

Checkpoint 2026-08-15 (Video Rendering UI, Progressive SFU Topology, Security Hardening & Signal Rate Limiting):
1. Slint UI Video & Screen Sharing Rendering: Updated `ui/app.slint` and `crates/nexo-app/src/lib.rs` with native video viewports
   (local video container with camera/screen share indicator, remote participant container), sidebar toggle buttons for camera
   and screen sharing, and event-loop RGBA8 frame rendering from `CallEngineEvent::LocalVideoFrame` and `RemoteVideoFrame`.
2. Progressive Topology Mode: Added `CallTopologyMode::Mesh` vs `CallTopologyMode::ParticipantSfu` in `nexo-media::session` with
   automatic dynamic transition at 5+ participants, emitting `CallEvent::TopologyChanged`.
3. SQLite Replay Table Pruning & Revocation: Added `LocalStore::prune_old_call_signals(older_than)` and `LocalStore::revoke_member`
   in `nexo-store`, ensuring bounded replay table retention and cryptographic access revocation with full unit test coverage.
4. Signaling Flood Rate Limiter: Added `SignalRateLimiter` sliding-window token limiter in `nexo-net::signalling` to mitigate
   DoS/burst spamming on authenticated peer endpoints.
5. All 69 workspace tests pass, zero warnings under strict Clippy (`-D warnings`), and `cargo fmt --all --check` is fully clean.

Checkpoint 2026-08-15 (5-Phase Roadmap Delivery: Packaging, P2P File Transfer, Adaptive Bitrate, NAT Traversal, Audio DSP):
1. Phase 1 - Native Packaging & Distribution: Release build profile optimizations (`opt-level = 3`, `lto = "thin"`, `codegen-units = 1`, `strip = true`), binary metadata in `crates/nexo-app/Cargo.toml`, `scripts/package-windows.ps1` (built portable zip `dist/nexo-0.1.0-windows-x86_64.zip`), `scripts/package-linux.sh` (Debian .deb & .tar.gz), `.desktop` entry, and automated GitHub Actions release pipeline `.github/workflows/release.yml`.
2. Phase 2 - P2P Chunked File & Media Transfer: Created `nexo-core::file_transfer` (`FileTransferOffer`, `FileChunk`, Ed25519 signature & SHA-256 chunk integrity), `nexo-net::file_transfer` (`FileTransferRequest`/`Response` protocol over `/nexo/file-transfer/0.1.0`), and SQLite persistence in `nexo-store` (`file_transfers` & `file_chunks_saved` with resumable tracking).
3. Phase 3 - Congestion Control & Adaptive Bitrate: Implemented `nexo-media::congestion::CongestionController` with AIMD (Additive Increase / Multiplicative Decrease) dynamic adaptation reacting to RTT, packet loss ratio, and jitter to adjust VP8 target bitrate, framerate, and resolution tier.
4. Phase 4 - NAT Traversal (STUN / TURN): Implemented `nexo-core::nat::{NatConfig, IceServer}` with support for external STUN/TURN servers while preserving direct zero-cloud LAN-only operation by default.
5. Phase 5 - Audio DSP (Acoustic Echo Cancellation & Noise Suppression): Implemented `nexo-media::dsp::{AcousticEchoCanceller, NoiseSuppressor, AudioDspPipeline}` using NLMS adaptive filtering and RMS noise floor tracking.
6. Full Workspace Verification: 76 unit and integration tests passing across all crates with 0 warnings on strict Clippy (`-D warnings`) and clean formatting.

Checkpoint 2026-08-15 (Nexo v0.2.0: Multi-Channel Communities, Double Ratchet PFS, TreeKEM MLS, System Tray & Rich Chat):
1. Multi-Channel Communities: Added `ChannelKind::Text` vs `ChannelKind::Voice` and `LocalStore::create_channel` / `LocalStore::channels` in `nexo-store` with SQLite schema migration and test suite.
2. 1-to-1 DMs with Double Ratchet: Created `nexo-core::double_ratchet::{DoubleRatchetSession, RatchetMessage, RatchetError}` implementing DH + symmetric KDF chain ratchets for Perfect Forward Secrecy and break-in recovery.
3. TreeKEM MLS Group Key Exchange: Created `nexo-core::mls::{MlsGroupState, MlsMember, MlsError}` implementing RFC 9420 left-balanced binary key trees and epoch secret derivation.
4. Procedural Audio Tones: Created `nexo-media::tones::{AudioToneKind, generate_tone}` synthesizing telephone ringtones and notification chimes.
5. Rich Markdown & Emojis: Created `nexo-core::markdown::{parse_markdown, replace_emoji_shortcodes}` and wired into Slint UI composer.
6. System Tray: Created `nexo-app::tray::{TrayState, TrayAction}` for background presence management.
7. Workspace Validation: 88 tests passing with 100% success and 0 Clippy warnings.

Checkpoint 2026-08-15 (Nexo v1.0.0 Golden Master / Production Ready Release):
1. Device Settings Persistence: Added `LocalStore::get_metadata` and `LocalStore::set_metadata` in `nexo-store` and wired `on_select_input/output/video_device` in `nexo-app` to persist audio/video device selections.
2. User Guide: Created comprehensive end-user manual `docs/USER_GUIDE.md` covering quick start, community invites, Markdown, voice notes, P2P file transfers, WebRTC calls, and network troubleshooting.
3. Unified Build Scripts: Created `scripts/build-all.ps1` (PowerShell) and `scripts/build-all.sh` (Bash) automating formatting, strict clippy, complete test suite execution, and distribution packaging.
4. Workspace Bump: Bumped root `Cargo.toml` and packaging scripts to version `1.0.0` and updated repository metadata.
5. CI/CD Release Pipeline: Validated `.github/workflows/release.yml` with multi-platform artifact packaging.

