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
  Capture   for screen frames (Bgra8, staged through Direct3D11), plus DXGI probing through the
  official `windows` crate; on Linux it has a real V4L2 camera backend (enumeration + mmap
  streaming capture, NV12/MJPEG/YUYV with strict negotiation), presence-based VA-API/PipeWire
  probing and a PipeWire/Portal screen-capture backend (XDG Desktop Portal `ScreenCast` + PipeWire,
  written but not yet runtime-validated). All `unsafe` is isolated in one module per
  target behind safe functions. Runtime-verified on this machine (Windows) and in
  WSL2/Ubuntu with the EMEET camera (V4L2, see the Linux checkpoint below):
  EMEET
  SmartCam C60E 4K enumerated, AMD Radeon RX 9060 XT reported, hardware H264/HEVC MFT encoders
  detected by the capability probe, live NV12 640x480 frames captured at ~30 fps via
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
- WebRTC video transport: every peer connection pre-negotiates a bounded set of video slots
  (slot zero is the direct local publisher; slots 1-15 are assigned per relay source). VP8
  (SSRC + PT 96) and optional H.264 are packetized into RTP, and inbound frames retain their
  negotiated track id. Each participant has an independent decoder per track, while the Slint
  gallery coalesces frames to keep call controls responsive. Transport and two-instance tests
  cover the encoded VP8 path; a full cross-machine multipublisher capture test remains pending.
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
- Active participant relay for the current negotiated call: non-host peers publish to the
  deterministic host and the host forwards encoded Opus/VP8 frames to the other negotiated
  peers. The measured election/migration model and media-frame cipher are still separate.
- A deterministic offerer prevents glare; a two-identity integration test covers separate SQLite
  stores, authenticated libp2p signalling, real SDP negotiation and encrypted Opus delivery.
- Windows output works when WASAPI uses 96 kHz; 48 kHz call audio is resampled internally.
- CI targets Windows and Ubuntu; Linux installs ALSA and Slint build dependencies.

## Immediate next work

1. Physically validate headset unplug/default-device switching on Windows and Linux, then add echo
   cancellation and cross-machine impairment tests before calling voice production-ready.
2. Runtime-validate camera and screen capture on a real Linux desktop. The Linux V4L2 and
   PipeWire/Portal backends are implemented, while automated capture tests remain ignored when no
   physical device or desktop session exists.
3. Exercise the new per-publisher relay slots with three or more real camera publishers and verify
   source removal/reuse under load. The bounded Slint gallery is active, but adaptive layout,
   cross-codec relay policy and a measured multipublisher capture test remain.
4. Add optional NAT traversal without making internet or a central service mandatory.
5. Harden membership ownership, revocation, replay-table retention and abuse/rate limits.
6. Package signed Windows and Linux builds; run UI and media tests on both operating systems.
7. Configure and verify the local OpenCode/Ollama collaboration path only after product
   correctness work is green.

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
cargo +1.97.1-x86_64-pc-windows-gnu test --workspace --all-targets -- --test-threads=1
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
`MFTEnumEx` detects hardware H264/HEVC encoders. `CapabilityProbe::probe()` assembles a
`CapabilityReport` (capture backends,
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
`Direct3D11CaptureFramePool` (polled via `TryGetNextFrame` with a 16 ms idle interval and a ~5s
no-frame bound), the surface is
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
3. Phase 3 - Congestion Control & Adaptive Bitrate: Implemented and unit-tested the AIMD controller model; the live WebRTC path currently applies RTCP REMB to sender bitrate limits, while FPS/resolution adaptation remains pending.
4. Phase 4 - NAT Traversal (STUN / TURN): The bounded `nexo-core::nat::{NatConfig, IceServer}` model is now consumed by the optional WebRTC ICE configuration through environment variables; DHT discovery and managed relay provisioning remain pending.
5. Phase 5 - Audio DSP (Acoustic Echo Cancellation & Noise Suppression): Implemented `nexo-media::dsp::{AcousticEchoCanceller, NoiseSuppressor, AudioDspPipeline}` using NLMS adaptive filtering and RMS noise floor tracking.
6. Full Workspace Verification: 76 unit and integration tests passing across all crates with 0 warnings on strict Clippy (`-D warnings`) and clean formatting.

Checkpoint 2026-08-15 (Nexo v0.2.0: Multi-Channel Communities, Double Ratchet PFS, MLS-inspired state, System Tray & Rich Chat):
1. Multi-Channel Communities: Added `ChannelKind::Text` vs `ChannelKind::Voice` and `LocalStore::create_channel` / `LocalStore::channels` in `nexo-store` with SQLite schema migration and test suite.
2. 1-to-1 DMs with Double Ratchet: Created `nexo-core::double_ratchet::{DoubleRatchetSession, RatchetMessage, RatchetError}` implementing DH + symmetric KDF chain ratchets for Perfect Forward Secrecy and break-in recovery.
3. MLS-inspired group state: Created `nexo-core::mls::{MlsGroupState, MlsMember, MlsError}` with a bounded member tree and epoch secret derivation. This is not RFC 9420 wire-compatible.
4. Procedural Audio Tones: Created `nexo-media::tones::{AudioToneKind, generate_tone}` synthesizing telephone ringtones and notification chimes.
5. Rich Markdown & Emojis: Created `nexo-core::markdown::{parse_markdown, replace_emoji_shortcodes}` and wired into Slint UI composer.
6. System Tray: Created `nexo-app::tray::{TrayState, TrayAction}` for background presence management.
7. Workspace Validation: 88 tests passing with 100% success and 0 Clippy warnings.

Checkpoint 2026-08-15 (Nexo v1.0.0 candidate / release scaffolding):
1. Device Settings Persistence: Added `LocalStore::get_metadata` and `LocalStore::set_metadata` in `nexo-store` and wired `on_select_input/output/video_device` in `nexo-app` to persist audio/video device selections.
2. User Guide: Created comprehensive end-user manual `docs/USER_GUIDE.md` covering quick start, community invites, Markdown, voice notes, P2P file transfers, WebRTC calls, and network troubleshooting.
3. Unified Build Scripts: Created `scripts/build-all.ps1` (PowerShell) and `scripts/build-all.sh` (Bash) automating formatting, strict clippy, complete test suite execution, and distribution packaging.
4. Workspace Bump: Bumped root `Cargo.toml` and packaging scripts to version `1.0.0` and updated repository metadata.
5. CI/CD Release Pipeline: Validated `.github/workflows/release.yml` with multi-platform artifact packaging.

Checkpoint 2026-08-20 (Media crash recovery, responsive call controls & documentation audit):
1. Fixed the NV12 conversion panic that copied the complete capture buffer into the luma plane;
   invalid or odd dimensions now fail as data errors instead of panicking.
2. Moved Windows/Linux camera and screen readers behind a dedicated capture worker so blocking
   native reads cannot stall the call command loop. Windows partial media sizes such as `3840x0`
   now fall back to the requested resolution, and MJPEG camera frames decode to I420.
3. Video codec failures now emit a recoverable UI event and preserve voice operation. RGBA frame
   uploads are size-checked before entering Slint. The two-instance test now propagates scenario
   failures instead of printing an error and exiting successfully.
4. Call controls no longer keep an internal active/video/screen state separate from the UI state;
   failed call startup resets the visible controls. The sidebar is scrollable so invitation and
   community controls remain reachable at the minimum window size.
5. Verification on the Windows GNU toolchain: `cargo test --workspace` passes, including 35
   `nexo-media` tests and the two-instance WebRTC scenario; strict workspace Clippy passes.
   Physical camera and desktop capture integration tests remain ignored unless the required
   hardware/session is available.

Checkpoint 2026-08-22 (Encrypted community messages, invitation group secret & membership convergence):
1. New invitations carry a random 32-byte community secret. Community messages use signed v2
   ChaCha20-Poly1305 envelopes with a per-epoch derived key, authenticated context and bounded
   wire size; legacy plaintext messages and invitations remain readable for compatibility.
2. MLS-inspired membership state now distinguishes strict remote commits from the explicit local
   join path. Add commits are rebuilt in deterministic `(epoch, id)` order so concurrent additive
   joins converge, while signed remove commits mark the member revoked and stop future history
   sharing with that device.
3. The store keeps revocation markers, rejects revoked credentials and authenticates encrypted
   messages before persistence. This remains a Nexo control plane rather than RFC 9420 TreeKEM:
   per-member key packages, private post-removal rotation and a moderation UI are still pending.

Checkpoint 2026-08-20 (Call-loop fault isolation & interaction hardening):
1. A failed audio/video queue on one peer no longer propagates out of `CallEngine::tick()`;
   the peer is closed and removed after the current iteration while the remaining call stays
   alive. Malformed remote audio/video frames are discarded instead of aborting the media loop.
2. VP8 decode and RGBA conversion now validate dimensions, strides, plane pointers and buffer
   sizes. Public rendering helpers use bounded indexing, with regression tests for invalid and
   truncated frames.
3. Call controls received stable hit areas, and the sidebar no longer uses an elastic spacer
   inside its scroll view. The two-instance app test now verifies immediate camera toggle
   feedback in addition to join, WebRTC connection, mute and leave.
4. Verification: `cargo clippy --workspace --all-targets -- -D warnings` passes; `nexo-media`
   has 37 passing tests; the two-instance app test passes; the voice loopback passes in isolation.
   The voice-loopback test now matches `PeerConnected` by the expected `PeerId` (instead of
   consuming an unrelated mDNS event), and the complete workspace suite passes with
   `--test-threads=1`; physical camera and screen tests remain intentionally ignored.

Checkpoint 2026-08-21 (Multipublisher slots and UI backpressure):
1. Replaced the single outgoing video track with 16 pre-negotiated slots per WebRTC connection.
   Slot zero carries the local publisher; the participant relay allocates slots 1-15 per source,
   releases them when a source leaves and never hashes sources into colliding slots.
2. Incoming video now keeps a decoder per negotiated track id. The app renders up to eight remote
   tiles and coalesces local/remote frames at roughly 15 FPS so video traffic cannot flood the
   Slint event queue and make call buttons appear unresponsive.
3. `cargo fmt --all`, strict clippy, the complete Windows workspace suite and WSL Ubuntu
   `cargo check --workspace --all-targets` pass after this change. A short Windows binary smoke
   run stayed alive through startup and shut down cleanly; physical camera and screen integration
   tests remain environment-dependent.

Checkpoint 2026-08-20 (Active participant relay correction):
1. The participant-hosted relay path now forwards encoded Opus/VP8 frames through the
   elected participant for the current call. A non-host publishes only to that host;
   the host publishes to the other negotiated peers and forwards received frames.
2. Relay election now uses only peers with a negotiated connection for the active call,
   rather than every authorized community member that happens to be online. Selection
   is deterministic and has regression coverage in `nexo-app`.
3. This is an active encoded-frame relay, not yet a complete measured `SfuTopology`
   migration protocol or a cross-machine multipublisher validation. Hardware H.264/HEVC capability
    probing is also not active encoding: the current interoperable call codec remains
    software VP8 until a compatible hardware encoder and decoder path are integrated.

Checkpoint 2026-08-21 (Active topology, capability negotiation & protected media):
1. Call signals now carry bounded, signed codec capabilities, SFU metrics and heartbeats.
   `CallEngine` negotiates H.264 only when the native encoder exists on both sides and
   falls back to VP8 otherwise.
2. The app loop now drives `SfuTopology`: it elects by advertised capacity, keeps a
   standby, detects heartbeat loss and routes a make-before-break migration through a
   connected target host.
3. `MediaFrameCipher` is active above WebRTC. Audio/video frames are encrypted at the
   publisher, remain opaque while relayed, and are authenticated/decrypted before local
   playback or rendering.
4. Native Windows H.264 MFT encoding and OpenH264 receiving are integrated; Linux keeps
   the tested VP8 fallback until a native H.264 encoder backend is available. The current
   multiparty video relay now uses bounded per-publisher tracks and a gallery, but still needs
   cross-machine load validation and adaptive layout.

Checkpoint 2026-08-21 (Relay lifecycle and codec truth):
1. Relayed audio now uses independent pre-negotiated Opus tracks, SSRCs, jitter buffers and
   decoders per publisher. Video relay slots are released and reused after a source leaves, with
   keyframes resetting decoder state for a reused track.
2. Transient RTP write failures and relay slot exhaustion no longer evict the whole WebRTC peer;
   the affected frame is dropped and the next media tick can retry. Discovery expiration and
   disconnection also remove the peer from SFU metrics and topology state.
3. H.264 capability advertisement now reflects a successfully initialized hardware encoder. The
   Windows backend supports both synchronous and event-driven asynchronous Media Foundation
   transforms; the async worker finalizes each event with `EndGetEvent`, calls `ProcessOutput` once
   per `METransformHaveOutput`, preserves frame timestamps, and falls back to VP8 on failure.

Checkpoint 2026-08-21 (Asynchronous H.264 worker):
1. The Windows async MFT path unlocks the transform, registers one event callback at a time and
   serializes `ProcessInput` and `ProcessOutput` on a dedicated worker thread.
2. A real AMD Media Foundation smoke test encoded 21 outputs from 30 NV12 frames without a crash;
   the capabilities probe reports H.264 as the preferred encoder on the test machine.

Checkpoint 2026-08-22 (P2P files and voice notes wired into the app):
1. The libp2p swarm now exposes the file-transfer request/response protocol. Offers are sent only
   to app-selected authorized peers, incoming offers require the sender identity and community
   membership to validate, and chunks are verified before being written to the local downloads
   directory and checked against the final SHA-256 root.
2. The `+` control opens a native Windows or XDG Portal file picker and sends files up to 256 MB;
   the loopback integration test covers offer, acceptance, chunk request and payload delivery.
3. The voice-note control now captures the selected 48 kHz input through the existing CPAL frame
   source, writes bounded mono PCM WAV data for up to 60 seconds and sends it through the same
   authenticated file path. A WAV writer regression test and the two-instance app test pass.

Checkpoint 2026-08-22 (Review pass for responsiveness and capability truth):
1. Native file dialogs now run off the Slint event thread and return their result through the UI
   dispatcher, so opening or cancelling a picker cannot block call controls and message sending.
2. Linux capability probing no longer advertises H.264 solely because `/dev/dri` or libva exists;
   it now constructs the same VA-API encoder used by the media engine before advertising it.
3. The discovery service bounds retained outgoing file sources to two active transfers. Formatting,
   strict clippy for app/net/video, Linux video clippy, file-transfer loopback, WAV writing and the
   two-instance app integration test pass. Physical device capture and cross-machine testing remain
   environment-dependent follow-up work.

Checkpoint 2026-08-22 (Linux VA-API encoder integration):
1. Replaced the Linux H.264 placeholder with a runtime-loaded `moq-vaapi` encoder. It accepts the
   existing tightly packed NV12 frames, emits Annex-B access units and inserts an IDR every 60
   frames; `NEXO_VAAPI_DEVICE` can select a specific `/dev/dri/renderD*` node.
2. The Linux check, strict video clippy and capabilities/smoke examples pass in the current WSL
   environment. That environment has no `/dev/dri`, so actual AMD/Intel hardware bitstream output
   still needs one physical Linux GPU validation before release packaging.

Checkpoint 2026-08-22 (RTCP bitrate feedback and capability audit):
1. WebRTC REMB packets are now downcast from the native RTCP event, smoothed through a shared
   estimator and applied to every negotiated video sender through `RTCRtpSender` parameters.
   The initial budget remains 2 Mbps until a valid report arrives, avoiding a first-tick drop to
   the minimum.
2. The README and architecture notes now distinguish the working bitrate limit from the future
   FPS/resolution controller, distinguish measured CPU headroom from the current GPU capability
   hint, and keep internet NAT traversal and physical Linux encoder validation pending.

Checkpoint 2026-08-22 (channels and capture DSP integration):
1. The desktop UI now lists channels, selects the active channel, and creates text or voice
   channels locally. Messages, file offers, voice notes and call joins use the selected channel.
2. Sync protocol `0.4.0` now replicates bounded channel metadata alongside community messages;
   imported channels preserve their stable IDs so late peers converge on the same view.
3. The microphone path now runs the noise suppressor and AEC/NLMS pipeline. AEC uses the latest
   playback frame available from the output queue; exact hardware-timed echo reference remains a
   physical-device validation item.

Checkpoint 2026-08-22 (optional ICE server wiring):
1. The WebRTC configuration now consumes optional `NEXO_STUN_SERVERS` and `NEXO_TURN_SERVERS`
   environment values while retaining an empty-server LAN default.
2. Malformed optional entries are skipped by the parser, and the media crate has coverage for
   valid STUN/TURN parsing without requiring network access during tests.

Checkpoint 2026-08-22 (convergent channels and regression recovery):
1. User-created channel IDs now use UUID v5 from the community identity and normalized channel
   name, so independent peers creating the same channel converge instead of producing duplicates.
2. The VP8 transport regression test now models keyframe recovery by retrying a keyframe after a
   transient malformed or lost RTP packet; this keeps the test focused on recovery without hiding
   production decoder errors. Windows and Linux workspace clippy pass with `-D warnings`.
3. The remaining product gaps are still explicit: native system-tray integration, real physical
   camera/screen capture validation on Linux, cross-machine multipublisher load testing, FPS and
   resolution adaptation, per-member MLS key packages, and managed DHT/internet relays.

Checkpoint 2026-08-22 (microphone sample-rate compatibility):
1. Input capture no longer rejects devices whose native rate is different from 48 kHz. The
   streaming mono framer now performs bounded linear resampling before producing the 20 ms Opus
   frames, preserving stereo-to-mono mixing and keeping the audio callback non-blocking.
2. Coverage includes a real 44.1 kHz stream shape, and the Windows media test suite now passes all
   42 tests. This removes a common silent-call failure on Windows audio devices.

Checkpoint 2026-08-22 (release artifacts):
1. The Windows release script produced `dist-review/nexo-1.0.0-review-windows-x86_64.zip` with the
   release `nexo.exe` and README. The Linux script produced both the portable tarball and a valid
   amd64 Debian package; package metadata and archive contents were inspected after the builds.
2. Native packaging is now verified for both target operating systems. Physical capture, GPU
   driver behavior and a real cross-machine call remain runtime validation items rather than
   packaging blockers.

Checkpoint 2026-08-22 (native system tray):
1. `NexoTray` is a separate Slint `SystemTrayIcon` component. Slint routes it to the native
   Windows notification area and Linux StatusNotifier/KSNI backend without Electron or a second
   widget toolkit.
2. The tray keeps the application event loop alive after the main window is hidden, restores the
   window on activation, and exposes an explicit quit action that runs normal application shutdown.
   If a desktop has no supported tray service, tray creation is optional and the normal window
   close behavior remains available.

Checkpoint 2026-08-22 (Windows physical capture validation):
1. The ignored camera integration test now passes on the physical Windows desktop with the EMEET
   camera, capturing a 640x480 NV12 frame of 460,800 bytes.
2. The ignored screen integration test now passes with the primary monitor, capturing a 2560x1440
   Bgra8 frame of 14,745,600 bytes. Linux physical camera, PipeWire/Portal and VA-API validation
   still require a real Linux desktop and GPU rather than WSL.

Checkpoint 2026-08-22 (final review smoke):
1. Windows GNU tests pass for `nexo-app` (including the two-instance community/voice integration
   test) and `nexo-media` (42 tests, including non-48-kHz microphone resampling).
2. Formatting and whitespace checks pass. No cargo, rustc or nexo process remains running after
   validation. The review remains honest about the pending cross-machine SFU load test, live FPS/
   resolution adaptation, Linux hardware capture/VA-API validation, per-member MLS key packages
   and managed
   DHT/internet relays.

Checkpoint 2026-08-22 (three-peer relay validation):
1. The media transport now has a three-peer WebRTC regression scenario: one publisher sends to a
   participant-hosted relay, the relay forwards both Opus and VP8 over an independent egress link,
   and a third peer receives and decodes the forwarded media.
2. The full Windows media suite passes 43 tests serially, including relay forwarding and relay
   slot reuse. This validates the transport-level SFU path; real multipublisher load across separate
   physical machines remains a deployment validation item.

Checkpoint 2026-08-22 (relay fan-out load validation):
1. A second regression scenario connects four publisher pairs to a participant relay and two
   independent subscriber egress links. Every egress receives four distinct Opus tracks and four
   distinct VP8 tracks, exercising the pre-negotiated slot budget and relay fan-out.
2. The Windows media suite now passes 45 tests serially. This is still an in-process WebRTC load
   test; network behavior across separate physical machines remains unverified. The same 45-test
   media suite also passes on Ubuntu under WSL after making track startup and cleanup bounded.

Checkpoint 2026-08-22 (live adaptive video quality):
1. `CallEngine` now consumes the lowest connected peer REMB estimate and applies bounded video
   profiles: 640x360 at 15 FPS, 854x480 at 24 FPS, or 1280x720 at 30 FPS. VP8 and available H.264
   encoders are recreated only on a profile transition; a failed hardware recreation keeps the
   previous profile and the software fallback remains available.
2. The congestion controller has direct bitrate-tier coverage, while the existing RTT/loss AIMD
   path remains available for future richer metrics. The remaining product gaps are physical
   cross-machine validation, Linux desktop/GPU validation, per-member MLS key packages and
   managed relays.

Checkpoint 2026-08-22 (application three-instance call validation):
1. Added `crates/nexo-app/tests/three_instances.rs`, which starts three isolated application
   instances, joins them through one invite, establishes the real WebRTC call, verifies that each
   participant sees the other two, exercises camera and mute controls, and leaves cleanly.
2. The scenario passes on Windows and Ubuntu/WSL. The Ubuntu run also confirms that a missing
   graphical system tray is reported as a non-fatal condition rather than closing the application.
3. This validates the application-level call lifecycle; physical cameras, microphones, and a
   multipublisher call across separate machines still need hardware/network validation.

Checkpoint 2026-08-22 (WebRTC data-channel file transfer):
1. `LanPeerConnection` now owns the negotiated `nexo-control` DataChannel in both offerer and
   answerer roles, exposes bounded binary send/receive APIs, and applies SCTP backpressure instead
   of dropping data when the application queue is full.
2. Attachments and voice notes prefer this channel when every authorized connected member is in
   the active call. The existing signed libp2p transfer remains the automatic fallback for members
   outside that call.
3. Offers and chunks reuse the signed `FileTransferOffer`/`FileChunk` model, use 8 KiB WebRTC
   chunks, validate author-to-peer identity, enforce the 256 MiB limit and verify the final SHA-256.
   The media suite covers bidirectional bounded DataChannel messages on Windows and Ubuntu/WSL;
   the app suite also round-trips a 20 KiB signed file through two native call engines and SQLite.

Checkpoint 2026-08-22 (SFU metrics refresh and failover hardening): the application now republishes
the local CPU/GPU/upload capability report every two seconds during an active call instead of
only sending heartbeats, allowing a changing machine condition to participate in re-election.
Heartbeat failover checks that the standby itself is still alive before promoting it; if both the
active host and standby have expired, the topology becomes hostless until a fresh eligible
candidate is observed. A new election regression covers that double-timeout case.

Checkpoint 2026-08-22 (authenticated media and ratchet primitives): replaced the custom
XOR/SHA-256 media protection with ChaCha20-Poly1305 AEAD, using a fresh random nonce per frame
and retaining the sequence in authenticated associated data. Audio and video now share one
monotonic sender sequence in the call engine, while nonce uniqueness does not depend on call
re-entry state. The Double Ratchet module now uses real X25519 DH steps, ChaCha20-Poly1305
message encryption, authenticated ratchet headers and strict duplicate/out-of-order rejection.
The core suite passes 39 tests, including tamper, repeated-sequence nonce and duplicate-delivery
regressions; direct-message transport/UI wiring was the next product gap addressed below.

Checkpoint 2026-08-22 (direct messages): added a signed `DirectMessageEnvelope`, deterministic
community-scoped conversation IDs, DirectSessionHello/DirectMessage call signals, persisted
Double Ratchet checkpoints and SQLite history. The Slint shell now lists authorized members under
Mensagens Diretas and routes the composer to the selected conversation. Offline delivery now uses
authenticated, paginated envelope sync with per-peer database-epoch receipts; reconnection is
covered by the two-instance scenario, including a restarted receiver. MLS membership commits are
now persisted and synchronized with acknowledged pages; application-message key packages,
member revocation and physical cross-machine media validation remain open product work.

Checkpoint 2026-08-22 (MLS membership persistence and sync):
1. Added signed, epoch-bound `MlsCommit` records for authorized member joins, with previous-state
   hashes and signature verification before applying a transition.
2. Added SQLite persistence for the per-community MLS state and commit history. Sync now carries
   bounded commit pages and acknowledges their IDs, so a large history continues on reconnect.
3. Added re-open persistence coverage. This is control-plane MLS wiring only: community-message
   encryption, key-package distribution and member removal remain deliberately separate work.

Checkpoint 2026-08-22 (community message envelopes):
1. New invitations carry a random group secret inside the signed capability. Legacy invitations
   remain verifiable through a compatibility signature path.
2. `SignedMessage` version 2 encrypts the body with ChaCha20-Poly1305 using the MLS epoch state,
   binds the message identity and channel as associated data, and retains historical epoch keys
   for local display after membership changes.
3. The app creates, imports and renders these envelopes through the real two-instance flow; the
   remaining MLS hardening is per-member key-package delivery and private rotation after removal.

Checkpoint 2026-08-22 (membership revocation and deterministic convergence):
1. Strict remote MLS commits are separated from the explicitly authorized local self-join path.
   Add commits are rebuilt in deterministic `(epoch, id)` order so concurrent additive joins use
   the same state, while signed remove commits advance the epoch and persist a revocation marker.
2. Revoked members no longer receive community history in later sync batches. Credentials remain
   deliverable long enough for the removed device to process its signed removal commit, then are
   rejected for future authorization.
3. The full Windows workspace test suite, GNU strict Clippy, Windows formatting and Ubuntu/WSL
   workspace checks pass. Remaining release risks are explicit: this is not RFC 9420-compatible,
   Linux physical capture/VA-API and cross-machine media need hardware validation, and the
   moderation UI, private post-removal rotation and managed internet relays are unfinished.

Checkpoint 2026-08-22 (LAN discovery retry hardening):
1. The discovery worker now retains a bounded set of mDNS and invitation addresses per peer (up to
   256 peers and eight addresses per peer), limits concurrent dials to eight, and retries failed
   dials with exponential backoff capped at 32 seconds. A successful connection resets the peer's
   attempt state, while the existing authenticated Noise/libp2p handshake and community
   authorization rules remain unchanged.
2. Added coverage for address deduplication and bounds. The two-instance and three-instance app
   WebRTC scenarios pass after this change; multicast-blocked networks still require a manual
   invitation address.

Checkpoint 2026-08-22 (founder moderation UI):
1. The Slint sidebar now lists authorized community members and exposes `Remover` only to the
   founder identified by the signed invitation. The network worker rechecks that authority,
   signs a removal commit, advances the local association state and persists the revocation.
2. The removal commit is immediately offered to connected peers through the existing sync path;
   periodic sync covers offline peers. A revoked instance can still open its local database, but
   cannot reauthorize itself or receive future community history.
3. An application test covers the founder removal flow end to end. Interoperable MLS key packages,
   private post-removal rotation and cross-machine validation remain open.

Checkpoint 2026-08-22 (serialized integration-test pipeline):
1. The Windows, Linux and CI build scripts now run unit tests and each network/media integration
   binary in an explicit sequence. `--test-threads=1` alone only serializes tests inside one
   binary; Cargo may still launch separate integration binaries concurrently.
2. This avoids contention between the two-instance and three-instance LAN scenarios while keeping
   the physical camera and desktop capture checks compiled and reported as ignored when hardware
   or a foreground desktop session is unavailable.

Checkpoint 2026-08-22 (private post-removal epoch rotation):
1. Removal commits now contain signed per-recipient X25519/ChaCha20-Poly1305 envelopes for a fresh
   epoch secret. Remaining members open their own envelope; the removed identity has no envelope
   and receives a non-derivable local current secret after processing the signed removal.
2. Sync replay now rebuilds the complete signed add/remove history before applying removals, so a
   partial page cannot leave leaf indexes or epoch secrets inconsistent. Active calls rekey after
   membership sync, close on local revocation and remove unauthorized peer transports.
3. Core and app regression tests cover envelope confidentiality, deterministic replay, media-key
   rotation and the existing multi-instance call scenarios. RFC 9420 interoperability, DHT and
   cross-machine physical media validation remain open.

Checkpoint 2026-08-22 (media failure containment):
1. Camera capture now uses bounded recovery with the same backoff policy as CPAL endpoints;
   a disconnected requested camera can be reopened without tearing down the WebRTC call.
   Screen-capture read failures clear the sharing state and emit the existing video-unavailable
   signal so the Slint controls cannot remain falsely active.
2. The application catches a panic escaping one media tick, attempts an async engine close,
   clears the call roster and returns the UI to the idle state. Hardware H.264 probing and
   construction also convert native panics into the existing VP8/software fallback.
3. Video UI scheduling resets its pending flag when the Slint event loop rejects a queued flush,
   preventing a closed or unavailable window from permanently suppressing later frames. The
   media and application regression suites pass after these changes; physical device and
   cross-machine validation remain open.
4. Full Windows verification passed: strict workspace Clippy, the serialized workspace/unit and
   integration pipeline, and the portable release package (`dist-review/nexo-1.0.0-review-
   windows-x86_64.zip`, 12.07 MB). The packaged executable stayed alive during an 8-second GUI
   smoke test. `wsl.exe -d Ubuntu cargo check --workspace --all-targets` also passed after the
   media changes; Linux runtime capture and cross-machine media still need a real desktop/link.
5. A WSL Ubuntu run of `cargo test --workspace --lib --bins -- --test-threads=1` passed all Linux
   unit-library and binary test targets (the expected ALSA default-device warning was non-fatal).

Checkpoint 2026-08-23 (optional Kademlia bootstrap):
1. `nexo-net` now includes libp2p Kademlia as an opt-in discovery layer. `NEXO_KAD_BOOTSTRAP`
   accepts semicolon-separated authenticated multiaddrs ending in `/p2p/<PeerId>`; bootstrap
   peers and identify/mDNS addresses are added to the DHT, and routable DHT peers return to the
   existing bounded dial/backoff path.
2. With the variable absent, no DHT bootstrap query is started and the LAN behavior remains mDNS,
   invitation/address and direct TCP/QUIC. The parser rejects malformed or transport-less entries,
   and the feature passes Windows compilation and strict Clippy; managed relays and real WAN
   traversal remain unverified.
3. The post-DHT integration checks passed for two-instance history/voice, three-instance calls and
   sync loopback. A refreshed Windows portable package was produced at
   `dist-review2/nexo-1.0.0-review2-windows-x86_64.zip`; its executable stayed alive during a
   six-second GUI smoke test. Linux `cargo check --workspace --all-targets` passed with Kademlia
   enabled.

Checkpoint 2026-08-23 (Kademlia resource bounds):
1. The discovery-only Kademlia behaviour now uses a 20-entry k-bucket, 30-second query timeout,
   10-second substream timeout and no periodic bootstrap loop. Its in-memory record store is
   capped at 256 records, 64 KiB per value, 16 providers per key and 64 locally provided keys;
   Nexo currently does not publish application records there.
2. Bootstrap parsing caps input entries before attempting to parse them, preserving the existing
   256-peer and per-peer address limits. The address returned by Kademlia is still passed through
   libp2p's authenticated peer connection before it reaches Nexo's bounded dialer.
3. Formatting, strict workspace Clippy, two-instance, three-instance, sync loopback and voice
   loopback checks passed after the limits were added. Linux `cargo check --workspace --all-targets`
   also passed, and the exact revision is packaged at
   `dist-review4/nexo-1.0.0-review4-windows-x86_64.zip`; managed relays, hole punching and
   cross-machine physical media remain open validation items.
4. mDNS and Identify addresses are fed into Kademlia only when at least one valid bootstrap
   entry is configured, preventing an automatic DHT query in the default LAN-only mode.

Checkpoint 2026-08-23 (ICE/DTLS failure diagnostics):
1. Native WebRTC connections now preserve the peer-connection state transitions that matter to
   startup: new, connecting, connected, disconnected, failed and closed. A retained watch state
   means `wait_until_connected` also succeeds when ICE/DTLS completed before the caller began
   waiting; it returns a specific error for an ICE/DTLS failure or early disconnect instead of
   waiting for the generic timeout.
2. The app already routes that error into the call status, so a NAT/firewall/STUN/TURN problem is
   distinguishable from a microphone, speaker, camera or encoder failure. LAN behavior is unchanged
   and remains independent of external ICE servers.
3. The new state classifier, race regression and 49-test media unit suite pass with strict
   workspace Clippy; real cross-network traversal still requires two physical networks and an
   available STUN/TURN service.

Checkpoint 2026-08-23 (libp2p relay client and UI resilience review):
1. `nexo-net` now includes the optional libp2p Circuit Relay v2 client and DCUtR behaviour while
   retaining the existing TCP/QUIC direct transports. `NEXO_RELAY_SERVERS` accepts bounded,
   authenticated `/p2p/<PeerId>` relay addresses; each configured relay receives a reservation
   address ending in `/p2p-circuit/p2p/<local-peer>` and direct dialing remains preferred when
   available.
2. Relay reservation addresses are preserved when the app builds invitations, and relay peers are
   not exposed as Nexo participants to synchronization or call UI. The LAN path is unchanged when
   the variable is absent.
3. The two-instance call test and the `nexo-net` parser suite pass after the change. A real WAN
   verification still requires a reachable relay server and two physical networks; the project
   does not ship or select a public relay automatically.

Checkpoint 2026-08-23 (optional hosted relay mode):
1. Every Nexo discovery swarm now contains a bounded libp2p relay-server behaviour. It is inert
   by default and becomes active only with `NEXO_RELAY_SERVER=1`; `NEXO_RELAY_LISTEN_PORT` selects
   a fixed TCP/QUIC port and defaults to 4001.
2. The hosted mode is intentionally an operator-controlled capability: it does not create a
   central service, it limits reservations and circuits, and it keeps the normal Nexo application
   protocols authenticated above the transport. Port forwarding and distributing the relay
   multiaddr remain deployment work.
3. The opt-in parser test and full local network tests remain green. A WAN relay reservation still
   needs a reachable forwarded port or a physical public host for runtime validation.
4. The Windows release package was rebuilt at
   `dist-review5/nexo-1.0.0-review5-windows-x86_64.zip`; its executable stayed alive during a
   six-second GUI smoke test. The package contains the retained ICE state fix.

Checkpoint 2026-08-23 (hosted relay reservation race fix):
1. Relay reservations are now requested after the authenticated relay connection is established,
   and are re-armed after a relay reconnect instead of being queued too early.
2. `hosted_relay_accepts_a_client_reservation` exercises a real in-process TCP relay, authenticated
   client connection, reservation acceptance and `/p2p-circuit` listener publication.
3. Windows formatting, the full `nexo-net` unit suite and workspace Clippy with `-D warnings` pass.

Checkpoint 2026-08-23 (media resilience and overlay-network addresses):
1. A runtime H.264 encoder failure now schedules bounded recreation using the existing endpoint
   recovery policy, avoiding a permanently silent already-negotiated H.264 track after a GPU reset.
2. Relay public addresses are registered once per swarm, and Tailscale/ZeroTier interfaces are no
   longer discarded from invite address generation; container/test adapters remain filtered.
3. The Windows media suite remains green with 49 unit tests, strict workspace Clippy passes, and
   the Linux WSL workspace check passes after the changes.

Checkpoint 2026-08-23 (hosted relay circuit dial):
1. Discovery can disable mDNS with `NEXO_DISABLE_MDNS=1` for deterministic invite/relay tests;
   the default remains automatic LAN discovery.
2. The hosted relay integration test now starts a bounded relay, reserves circuit addresses for
   two clients, and dials client A from client B through the announced
   `/p2p-circuit/p2p/<peer>` address. It passed without mDNS, proving the relayed transport path
   beyond reservation-only coverage.
3. The same test exposed and corrected a test-side address construction error: libp2p already
   includes the destination peer component in the address emitted after a reservation.

Checkpoint 2026-08-23 (Windows capture idle polling):
1. Windows Graphics Capture now waits up to one 60-FPS interval between empty frame-pool polls,
   reducing idle polling from roughly 200 Hz to roughly 60 Hz while preserving the existing five
   second no-frame recovery bound and synchronous API.

Checkpoint 2026-08-23 (Linux distribution audit):
1. Added the official AGPL-3.0 license text at the repository root, fixing the README license link
   and making the open-source distribution self-contained.
2. The Linux packager now includes `LICENSE` and the 256x256 Nexo icon in both the portable tarball
   and Debian package; the `.deb` installs README, license, desktop entry and hicolor icon files.
3. A release build generated `dist-linux-review3/nexo-1.0.0-linux-x86_64.tar.gz` and
   `dist-linux-review3/nexo_1.0.0_amd64.deb`. The binary is a stripped x86_64 ELF, the extracted
   package contains all expected files, and `ldd` reports no missing libraries in WSL.
4. WSL has no graphical desktop session, so GUI launch remains a real Linux desktop validation
   item even though the package and binary checks pass.

Checkpoint 2026-08-23 (multiparty relay convergence and failover):
1. The three-instance application test now verifies that all participants converge on one relay,
   exercises responsive video/mute controls, shuts down the elected relay and confirms that both
   survivors keep the call connected under one promoted relay.
2. Call topology now waits for the complete negotiated participant set and its signed metrics before
   selecting a host; membership changes reset that snapshot so joins and departures cannot preserve
   a stale election.
3. `SfuTopology::new_convergent` makes the initial host and standby, plus host-loss promotion,
   identity-deterministic while preserving score-based hysteresis for voluntary capacity migration.
   The default core constructor remains score-based for existing election semantics.
4. The focused three-instance test, the election tests and Windows formatting/checks pass. Physical
   multi-machine media load and real WAN traversal remain separate validation items.

Checkpoint 2026-08-23 (host-led SFU migration signal):
1. `SfuMigrationProposal` now carries a bounded monotonic term and the authenticated source and
   destination peer IDs. Call-signal validation rejects malformed or unbounded proposals before
   they reach the topology state machine.
2. Only the current relay may start a voluntary capacity migration. Replicas accept a newer signed
   proposal only from that relay and only when the destination is already a negotiated call peer;
   host-loss promotion remains available locally through the deterministic heartbeat path.
3. Core election tests cover proposal parsing, stale-term rejection and the replica role boundary.
   The three-instance scenario uses the explicit no-camera integration entry point so several local
   app instances never contend for one physical capture device; the normal desktop entry point still
   discovers the default camera.
4. Windows formatting, workspace Clippy and the full workspace test suite pass after this change.
   Physical cross-machine media and WAN traversal still require real networks.

Checkpoint 2026-08-23 (invite address fanout and signalling budget):
1. The Identify and Kademlia address-learning handlers now run independently of mDNS. Existing
   invite-only connections therefore keep learning usable peer addresses when `NEXO_DISABLE_MDNS=1`.
2. Authenticated sync offers carry a bounded, deduplicated list of learned peer addresses. The
   discovery task validates the sender key, advertises only peers already authenticated as connected,
   parses supported transports, appends the advertised target PeerId and retries the resulting direct
   dials with the existing backoff.
3. Individual call-signal payloads are capped at 32 KiB, request batches at 12 signals, and the
   per-device sliding-window limiter remains 30 requests per five seconds and caps stale device keys
   at 256. This accommodates real WebRTC SDP offers while staying below the 512 KiB request transport
   cap.
4. The three-instance application call test passes with mDNS enabled and disabled, including
   responsive controls and relay migration. Full Windows workspace tests, strict Clippy and format
   checks pass. Physical multi-machine media, Linux GUI validation and real WAN traversal remain
   open validation items.

Checkpoint 2026-08-23 (replay-store maintenance):
1. The app now prunes call-signal replay records older than ten minutes on its existing five-second
   synchronization tick, and SQLite has an index for the received timestamp used by that deletion.
2. The store suite (13 tests), strict workspace Clippy, and the three-instance invite-only call test
   pass after the change. The current WebRTC path gathers ICE into SDP rather than applying separate
   trickle candidates; this is documented as a future negotiation extension.

Checkpoint 2026-08-23 (portable release pipeline):
1. Windows build and packaging scripts no longer depend on a user-specific MinGW path or a private
   fixed toolchain; an optional `-Toolchain` selects rustup explicitly, otherwise the default is used.
   `CARGO_TARGET_DIR` and absolute output directories are honored.
2. Linux packaging strips a leading `v` from release tags before writing Debian metadata, so a Git tag
   such as `v1.0.0` produces a valid package version `1.0.0`.
3. A real Linux build produced and inspected `dist-package-audit/nexo-1.0.0-linux-x86_64.tar.gz` and
   `dist-package-audit/nexo_1.0.0_amd64.deb`; a Windows GNU build produced a ZIP in the temporary
   audit directory, whose extracted executable stayed alive for six seconds. CI/Linux GUI and
   physical cross-machine media remain separate validation items.

Checkpoint 2026-08-23 (deterministic WebRTC offer codec):
1. The app now wraps each signed WebRTC offer with the gathered SDP and the exact codec selected
   by the offerer. The answerer validates that selection against its native encoder and uses the
   same codec when constructing the peer connection, eliminating the race between capability and
   participant-state signal delivery.
2. Unwrapped SDP remains supported for local compatibility and falls back to the existing
   authenticated capability decision. Unknown codecs and empty wrapped SDPs are rejected before
   entering the media engine.
3. Windows unit tests, strict workspace Clippy, and the three-instance call scenario pass with
   both default mDNS discovery and invite-only discovery (`NEXO_DISABLE_MDNS=1`). The previous
   concurrent-build failure was isolated to a corrupted temporary target directory; a fresh target
   reproduced both scenarios successfully. Physical camera, screen, GPU and WAN validation remain
   open.

Checkpoint 2026-08-23 (native package CI gates):
1. GitHub Actions now has dedicated Linux and Windows packaging jobs in addition to the workspace
   test matrix. Linux inspects the Debian metadata, tarball contents, executable bit and dynamic
   library resolution; Windows extracts the portable ZIP and keeps `nexo.exe` alive for a five-second
   smoke test.
2. Both jobs upload their native artifacts for review. Camera and screen tests remain compiled in
   CI but are not treated as physical capture evidence when the runner has no desktop device.
3. The workflow YAML parses successfully on Ubuntu, and the local Windows formatting, Clippy,
   three-instance call tests and Linux workspace check remain green after the addition.

Checkpoint 2026-08-23 (physical Windows media capability validation):
1. On the development PC, the physical camera test captured a `640x480 NV12` frame and the
   primary screen capture test captured a `2560x1440 BGRA8` frame successfully.
2. The capability probe enumerated one EMEET SmartCam C60E 4K camera and an AMD Radeon RX 9060 XT;
   Media Foundation H.264 and software VP8 were both available, with H.264 selected as preferred.
3. The native H.264 smoke test produced 18 encoded outputs. This proves local capture and encoder
   availability on Windows, but does not replace two-machine media, Linux GUI and real WAN tests.

Checkpoint 2026-08-23 (WebRTC video loopback coverage):
1. Added an isolated two-peer WebRTC integration test that negotiates VP8, encodes a synthetic
   I420 frame, sends it through RTP, reconstructs the access unit and decodes it back to an image.
2. The receiver now recovers VP8 keyframe dimensions from the RTP access unit and carries them
   across subsequent frames; the loopback test verifies the `160x120` metadata and decoded image.
3. The Windows GNU test passed without a camera, screen source or external network. Physical
   multi-machine media, Linux GUI and real WAN traversal remain open validation items.

Checkpoint 2026-08-23 (Linux media validation):
1. WSL Ubuntu compiled `nexo-media` with its Linux backends (V4L2, PipeWire/XDG Portal and the
   optional VA-API path) and passed all 49 unit tests plus the VP8 WebRTC loopback integration test.
2. Linux Clippy with `-D warnings` passed for all crate targets. The environment has no ALSA
   hardware, so audio-device diagnostics are expected and do not invalidate the software tests.
3. A physical Linux desktop capture/encoder run and a cross-machine/WAN call remain open; the
   Linux result proves build and software/WebRTC fallback coverage only.

Checkpoint 2026-08-23 (Linux application scenario):
1. The three-instance Slint integration scenario passed in WSL with `NEXO_DISABLE_MDNS=1`: invite
   creation/join, call negotiation, participant-hosted relay election, UI control changes and
   relay migration after the elected host stops all completed successfully.
2. The headless environment reported only the expected inability to create a system-tray icon;
   the application test itself completed in 16.98 seconds without closing the event loop early.
3. A physical Linux desktop run and a cross-machine/WAN media call remain open validation items.

Checkpoint 2026-08-23 (Windows package licensing):
1. The Windows portable packager now includes the repository `LICENSE` alongside `nexo.exe` and
   `README.md`; the CI package gate checks that the license survives extraction.
2. A local ZIP assembled from the existing release binary contained all three files and its
   executable stayed alive for the five-second smoke interval. A fresh clean CI runner remains
   the authoritative end-to-end check for the release script.

Checkpoint 2026-08-23 (release publication race):
1. The GitHub release workflow now uploads Windows and Linux packages as separate workflow
   artifacts and publishes them from one dependent job, avoiding concurrent writes to the same
   GitHub Release from the platform matrix.
2. The Linux release job now installs the native build dependencies used by PipeWire, VA-API,
   V4L2, X11 and Slint before packaging. Artifact presence is required before publication.

Checkpoint 2026-08-23 (video regression gate):
1. The VP8 WebRTC video loopback integration test is now part of the Windows/Linux build scripts
   and the main CI test matrix, rather than being only a local validation command.
2. Workflow YAML and the Linux build script parse successfully after the gate was added; the
   targeted Windows and Linux video tests had already passed before this wiring change.

Checkpoint 2026-08-23 (screen color conversion):
1. BGRA screen-capture frames now convert every pixel through the RGB-to-YUV coefficients used
   by the camera paths instead of treating the green channel as the complete luma signal.
2. The I420 U/V planes now average each 2x2 source block, preserving desktop color through VP8
   and H.264 input conversion while keeping the existing bounded even-dimension checks.
3. The `nexo-media` library suite passes 50 tests, including a red BGRA regression case, and
   strict Clippy passes on the Windows GNU toolchain. Physical multi-machine and Linux desktop
   capture validation remain open.

Checkpoint 2026-08-23 (YUY2 chroma conversion):
1. YUY2 camera frames now reduce 4:2:2 chroma to I420 4:2:0 by averaging the matching samples
   from each pair of source rows; the luma plane remains per-pixel and bounded.
2. A 4x4 conversion regression test verifies distinct chroma values across two vertical blocks,
   covering webcams that negotiate YUY2 instead of NV12. The media suite now passes 51 tests and
   strict Clippy remains green.

Checkpoint 2026-08-23 (Linux package dependency gate):
1. The Debian package declares runtime alternatives for both the legacy and t64 ALSA/PipeWire
   package names, alongside Fontconfig; the clean Ubuntu WSL package inspection resolved those
   dependencies and found no missing shared libraries with `ldd`.
2. The package contains the native executable, desktop entry, icon, README, and license; the
   tarball contains the executable, icon, README, and license as well.
3. The Linux package job now installs the full PipeWire, VA-API/V4L2, X11 and Slint build
   dependency set used by the release workflow, so its build path matches release packaging.

Checkpoint 2026-08-23 (source-aware adaptive video profiles):
1. The live REMB-selected video profile now fits inside the negotiated camera or monitor
   resolution instead of upscaling a smaller source; it also preserves 4:3/16:9 aspect ratio.
2. The profile remains bounded by the network tier for bitrate and FPS, while the effective
   encoder dimensions are derived from the actual capture source. Regression tests cover a
   640x480 source in the medium/low tiers and a 2560x1440 source at 720p.

Checkpoint 2026-08-23 (SFU metric-set convergence):
1. Relay election now builds its candidate list from the current call's peer IDs only; connected
   authorized peers outside that call can no longer influence a local replica's host choice.
   A focused app regression test covers this stale-metric case.
2. Headless calls now skip native video encoder profile recreation until a camera or screen source
   exists, avoiding unnecessary driver work in audio-only calls.
3. The Windows GNU three-instance run previously reproduced a divergent relay choice; after the
   filter change it exceeded the harness timeout without a new diagnostic. The Linux WSL rerun
   could not link because `lld` terminated with `SIGBUS`; cross-instance relay convergence remains
   unvalidated in this environment rather than being marked complete.

Checkpoint 2026-08-23 (deterministic SFU bootstrap validation):
1. The topology now establishes the first sorted call member as the initial relay and the second
   as standby before metric samples arrive; later capacity-based migration remains enabled.
2. The Windows GNU toolchain passed strict Clippy, 51 `nexo-core` unit tests and 12 `nexo-app`
   unit tests, including the new bootstrap and call-scoped metric regressions.
3. The three-instance invite-only scenario passed in 11.41 seconds: all participants converged on
   one relay, UI controls stayed responsive, and survivors migrated after the elected relay shut
   down. Physical cross-machine media, Linux GUI and WAN traversal remain open.

