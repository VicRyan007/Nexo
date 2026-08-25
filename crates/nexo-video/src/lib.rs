//! Video capture and platform capability probing for Nexo.
//!
//! # Safety exception
//!
//! The workspace forbids `unsafe` code (see `AGENTS.md`). This crate is the
//! single documented exception: the Windows backend in [`platform::windows`]
//! talks to Media Foundation (camera enumeration and hardware MFT encoder
//! detection) and DXGI (GPU description) through the official `windows` crate.
//! Every native call is isolated inside that one
//! module behind safe, panic-free functions; all other modules of this crate
//! (and of the rest of the workspace) remain `unsafe`-free.
//!
//! The crate has no dependencies on other `nexo-*` crates so it stays a leaf:
//! `nexo-app` and `nexo-media` consume it and map its report onto their own
//! capability model.

mod capture;
mod devices;
mod encoder;
mod frame_worker;
mod platform;
mod probe;
mod screen;

pub use capture::{PixelFormat, VideoCaptureSource, VideoFrame};
pub use devices::{VideoDeviceInfo, VideoError, enumerate_cameras};
pub use encoder::{EncodedH264Frame, HardwareH264Encoder};
pub use probe::{
    AccelerationApi, CapabilityProbe, CapabilityReport, CaptureBackend, CodecCapability, MediaKind,
};
pub use screen::{MonitorInfo, ScreenCaptureSource, enumerate_monitors};
