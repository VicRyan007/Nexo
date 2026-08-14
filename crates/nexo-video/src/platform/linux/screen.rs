//! Linux screen capture: XDG Desktop Portal `ScreenCast` + `PipeWire`.
//!
//! The portal handshake (`ashpd`) runs on a short-lived tokio runtime; once it
//! returns the `PipeWire` node id and a socket, a `ThreadLoopRc` owns a
//! dedicated `PipeWire` loop that streams the node into a [`ScreenCapture`].
//! Monitor enumeration goes through the X11 `RandR` extension (`x11rb`); on
//! pure `Wayland` or headless sessions a single pseudo-monitor is reported so
//! the portal picker can still be reached.

use std::os::fd::OwnedFd;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use ashpd::desktop::{
    CreateSessionOptions, PersistMode,
    screencast::{
        CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
        StartCastOptions, Stream as ScreencastStream,
    },
};
use pipewire as pw;
use pw::{
    properties::properties,
    spa::{
        param::{
            ParamType,
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
        },
        pod::Pod,
        utils::Direction,
    },
};
use x11rb::{
    connection::Connection as X11Connection,
    protocol::{randr, xproto},
    rust_connection::RustConnection,
};

use crate::capture::{PixelFormat, VideoFrame};
use crate::devices::VideoError;
use crate::screen::MonitorInfo;

/// How long [`ScreenCapture::read_frame`] waits for the next frame before
/// reporting "no frame yet", keeping callers responsive on idle streams.
const FRAME_POLL: Duration = Duration::from_millis(100);

// --- monitor enumeration ---------------------------------------------------

/// Enumerate monitors through X11/RandR (works on X11 and `XWayland`
/// sessions).
///
/// When no X server is reachable (pure `Wayland` without `XWayland`, or
/// headless) a single pseudo-monitor is reported so callers still reach the
/// portal picker; the real size arrives once streaming starts.
#[allow(clippy::unnecessary_wraps)] // required by the crate's platform trait
pub(crate) fn enumerate_monitors() -> Result<Vec<MonitorInfo>, VideoError> {
    match x11_monitors() {
        Ok(monitors) => Ok(monitors),
        Err(_) => Ok(vec![MonitorInfo {
            id: "primary".into(),
            name: "Tela principal (selecao no portal)".into(),
            is_primary: true,
            width: 0,
            height: 0,
        }]),
    }
}

fn x11_monitors() -> Result<Vec<MonitorInfo>, VideoError> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| VideoError::screen_capture(format!("x11 indisponivel: {e}")))?;
    let root = conn.setup().roots[screen_num].root;
    let reply = randr::get_monitors(&conn, root, true)
        .map_err(|e| VideoError::screen_capture(format!("randr indisponivel: {e}")))?
        .reply()
        .map_err(|e| VideoError::screen_capture(format!("randr falhou: {e}")))?;
    let mut monitors = Vec::with_capacity(reply.monitors.len());
    for monitor in reply.monitors {
        let name = atom_name(&conn, monitor.name);
        monitors.push(MonitorInfo {
            id: name.clone(),
            name,
            is_primary: monitor.primary,
            width: u32::from(monitor.width),
            height: u32::from(monitor.height),
        });
    }
    Ok(monitors)
}

/// Resolve an X11 atom to its string name, with a best-effort fallback.
fn atom_name(conn: &RustConnection, atom: xproto::Atom) -> String {
    xproto::get_atom_name(conn, atom)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map_or_else(
            || "Monitor".into(),
            |reply| String::from_utf8_lossy(&reply.name).into_owned(),
        )
}

// --- screen capture ---------------------------------------------------------

/// Negotiated stream parameters shared between the `PipeWire` callbacks and
/// the public accessors.
#[derive(Clone, Copy, Debug)]
struct FormatState {
    resolution: (u32, u32),
    format: PixelFormat,
}

/// Callback user data for the `PipeWire` stream listener.
struct CaptureData {
    sender: mpsc::Sender<VideoFrame>,
    format: VideoInfoRaw,
    state: Arc<Mutex<FormatState>>,
    started: Instant,
}

/// Live screen capture of one portal-selected monitor.
///
/// Fields are ordered so that `_listener` (which unregisters the callbacks)
/// drops before `_stream` (which tears the `PipeWire` stream down), and
/// `thread_loop` (which owns the `PipeWire` loop) drops last. [`Drop`] stops
/// the loop thread first so no callback runs during teardown.
pub(crate) struct ScreenCapture {
    _listener: pw::stream::StreamListener<CaptureData>,
    _stream: pw::stream::StreamRc,
    thread_loop: pw::thread_loop::ThreadLoopRc,
    rx: mpsc::Receiver<VideoFrame>,
    state: Arc<Mutex<FormatState>>,
}

impl ScreenCapture {
    /// Open a screen capture stream through the `ScreenCast` portal.
    ///
    /// The portal shows the system picker; `monitor_id` is passed as a restore
    /// token so portals that remember it can pre-select the same monitor.
    pub(crate) fn open_monitor(monitor_id: &str) -> Result<Self, VideoError> {
        let (portal_stream, fd) = portal_handshake(monitor_id)?;

        pw::init();

        // SAFETY: `ThreadLoopBox::new` wraps `pw_thread_loop_new` and checks
        // the result for null; `None` name and properties are always valid.
        let thread_loop =
            unsafe { pw::thread_loop::ThreadLoopRc::new(None, None) }.map_err(pw_error)?;
        let context = pw::context::ContextRc::new(&thread_loop, None).map_err(pw_error)?;
        let core = context.connect_fd_rc(fd, None).map_err(pw_error)?;

        let (sender, rx) = mpsc::channel();
        let (width, height) = portal_stream.size().map_or((0, 0), |(w, h)| {
            (
                u32::try_from(w).unwrap_or_default(),
                u32::try_from(h).unwrap_or_default(),
            )
        });
        let state = Arc::new(Mutex::new(FormatState {
            resolution: (width, height),
            format: PixelFormat::Unknown,
        }));
        let data = CaptureData {
            sender,
            format: VideoInfoRaw::default(),
            state: state.clone(),
            started: Instant::now(),
        };

        let stream = pw::stream::StreamRc::new(
            core,
            "nexo-screen-capture",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(pw_error)?;

        let listener = stream
            .add_local_listener_with_user_data(data)
            .param_changed(on_param_changed)
            .process(on_process)
            .register()
            .map_err(pw_error)?;

        let values = serialize_format_params((width, height))?;
        let pod = Pod::from_bytes(&values)
            .ok_or_else(|| VideoError::screen_capture("parametro de formato invalido"))?;
        let mut params = [pod];

        let lock = thread_loop.lock();
        stream
            .connect(
                Direction::Input,
                Some(portal_stream.pipe_wire_node_id()),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(pw_error)?;
        drop(lock);
        thread_loop.start();

        Ok(Self {
            _listener: listener,
            _stream: stream,
            thread_loop,
            rx,
            state,
        })
    }

    /// The resolution actually negotiated (seeded from the portal size and
    /// refined by the `PipeWire` `Format` param once the stream starts).
    #[must_use]
    pub(crate) fn resolution(&self) -> (u32, u32) {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resolution
    }

    /// Pull the next captured frame without buffering more than one.
    ///
    /// Returns `Ok(None)` when no new frame arrived within [`FRAME_POLL`], so
    /// callers pacing a video clock never block on a stalled stream.
    pub(crate) fn read_frame(&mut self) -> Result<Option<VideoFrame>, VideoError> {
        match self.rx.recv_timeout(FRAME_POLL) {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(VideoError::screen_capture("stream de captura encerrada"))
            }
        }
    }
}

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        // Stop the loop thread before the stream objects are destroyed, so no
        // callback can touch them during teardown.
        self.thread_loop.stop();
    }
}

/// Record the negotiated raw format and resolution once the stream reports it.
fn on_param_changed(
    _stream: &pw::stream::Stream,
    user_data: &mut CaptureData,
    id: u32,
    param: Option<&Pod>,
) {
    let Some(param) = param else {
        return;
    };
    if id != ParamType::Format.as_raw() {
        return;
    }
    let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param) else {
        return;
    };
    if media_type != MediaType::Video || media_subtype != MediaSubtype::Raw {
        return;
    }
    if user_data.format.parse(param).is_err() {
        return;
    }
    let mut state = user_data
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let size = user_data.format.size();
    state.resolution = (size.width, size.height);
    state.format = map_video_format(user_data.format.format());
}

/// Copy the latest captured buffer into a frame and hand it to the channel.
fn on_process(stream: &pw::stream::Stream, user_data: &mut CaptureData) {
    let Some(mut buffer) = stream.dequeue_buffer() else {
        return;
    };
    let datas = buffer.datas_mut();
    let Some(data) = datas.first_mut() else {
        return;
    };
    let offset = usize::try_from(data.chunk().offset()).unwrap_or_default();
    let size = usize::try_from(data.chunk().size()).unwrap_or_default();
    let Some(bytes) = data.data() else {
        return;
    };
    if size == 0 || offset.saturating_add(size) > bytes.len() {
        return;
    }
    let slice = &bytes[offset..offset + size];

    let negotiated = user_data.format.size();
    if negotiated.width == 0 || negotiated.height == 0 {
        return;
    }
    let frame = VideoFrame {
        width: negotiated.width,
        height: negotiated.height,
        format: map_video_format(user_data.format.format()),
        timestamp: user_data.started.elapsed(),
        data: slice.to_vec().into_boxed_slice(),
    };
    let _ = user_data.sender.send(frame);
}

/// Best-effort mapping from SPA video formats to the crate's `PixelFormat`.
///
/// `BGRx` carries the same 4 bytes/pixel layout as `BGRA` (the fourth byte is
/// unused padding), so both map to [`PixelFormat::Bgra8`].
fn map_video_format(format: VideoFormat) -> PixelFormat {
    match format {
        VideoFormat::BGRx | VideoFormat::BGRA => PixelFormat::Bgra8,
        VideoFormat::YUY2 => PixelFormat::Yuy2,
        _ => PixelFormat::Unknown,
    }
}

// --- portal handshake ------------------------------------------------------

fn portal_handshake(monitor_id: &str) -> Result<(ScreencastStream, OwnedFd), VideoError> {
    let runtime = tokio::runtime::Runtime::new()
        .map_err(|e| VideoError::screen_capture(format!("runtime tokio: {e}")))?;
    runtime.block_on(async {
        let proxy = Screencast::new().await.map_err(portal_error)?;
        let session = proxy
            .create_session(CreateSessionOptions::default())
            .await
            .map_err(portal_error)?;
        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_cursor_mode(CursorMode::Hidden)
                    .set_sources(SourceType::Monitor | SourceType::Window)
                    .set_multiple(false)
                    .set_restore_token(Some(monitor_id))
                    .set_persist_mode(PersistMode::DoNot),
            )
            .await
            .map_err(portal_error)?;
        let response = proxy
            .start(&session, None, StartCastOptions::default())
            .await
            .map_err(portal_error)?
            .response()
            .map_err(portal_error)?;
        let stream =
            response.streams().first().cloned().ok_or_else(|| {
                VideoError::screen_capture("nenhum monitor selecionado no portal")
            })?;
        let fd = proxy
            .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
            .await
            .map_err(portal_error)?;
        Ok((stream, fd))
    })
}

#[allow(clippy::needless_pass_by_value)] // error adapters used directly as map_err fns
fn portal_error(error: ashpd::Error) -> VideoError {
    VideoError::screen_capture(format!("portal: {error}"))
}

#[allow(clippy::needless_pass_by_value)] // error adapters used directly as map_err fns
fn pw_error(error: pw::Error) -> VideoError {
    VideoError::screen_capture(error.to_string())
}

/// Serialize the SPA format param the `PipeWire` stream must negotiate.
/// Prefers
/// `BGRx` (the layout the portal implementations deliver) with `YUY2` as a
/// bandwidth-cheap raw fallback.
fn serialize_format_params(size: (u32, u32)) -> Result<Vec<u8>, VideoError> {
    let (width, height) = size;
    let width = width.max(1);
    let height = height.max(1);
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pw::spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pw::spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pw::spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::YUY2,
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width, height },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 8192,
                height: 8192,
            }
        ),
        pw::spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 25, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    let (out, _) = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .map_err(|e| VideoError::screen_capture(format!("parametro de formato: {e}")))?;
    Ok(out.into_inner())
}
