//! Nexo desktop application: Slint shell, identity, store and orchestration.
//!
//! The binary entry point (`main.rs`) is a thin wrapper around [`run`]; tests
//! can construct one or more isolated application instances with [`start_app`].

slint::include_modules!();

pub mod tray;
pub use tray::{TrayAction, TrayState};

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result};
use base64::{
    Engine as _,
    engine::general_purpose::{STANDARD as BASE64, URL_SAFE_NO_PAD},
};
use chrono::{DateTime, Local};
use futures::FutureExt;
use nexo_core::{
    CallNegotiationRole, CallSignal, CallSignalKind, CommunityCredential, DeviceIdentity,
    DirectMessageEnvelope, DirectSessionHello, DoubleRatchetSession, DoubleRatchetState,
    ElectionPolicy, FileChunk, FileTransferOffer, MlsCommit, MlsCommitOperation, MlsGroupState,
    NetworkInvite, NodeMetrics, SfuMigrationProposal, SfuMigrationState, SfuTopology,
    SfuTopologyEvent, SignedMessage, call_negotiation_role, compute_sha256, current_timestamp,
    derive_initial_private, direct_conversation_id,
};
use nexo_media::{CallEngine, CallEngineEvent, DATA_CHANNEL_MESSAGE_BYTES, VideoCodec};
use nexo_net::{
    CommunityAck, CommunitySync, DiscoveryEvent, DiscoveryService, FileOfferResponseChannel,
    FileTransferResponse, SignalRequest, SyncChannel, SyncRequest,
    sync::{
        MAX_DIRECT_MESSAGES_PER_COMMUNITY, MAX_MESSAGES_PER_COMMUNITY,
        MAX_MLS_COMMITS_PER_COMMUNITY,
    },
};
use nexo_store::{Channel, ChannelKind, Community, LocalStore};
use serde::{Deserialize, Serialize};
use slint::{Model, ModelRc, SharedString, VecModel};
use sysinfo::System;

const HISTORY_LIMIT: usize = 200;
const MAX_RENDERED_REMOTE_VIDEOS: usize = 8;
const VIDEO_UI_INTERVAL: Duration = Duration::from_millis(66);
const WEBRTC_FILE_CHUNK_SIZE: u32 = 8 * 1024;

#[derive(Clone, Debug)]
struct PendingVideoFrame {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Default)]
struct PendingVideoUi {
    local: Option<PendingVideoFrame>,
    remotes: HashMap<String, PendingVideoFrame>,
    scheduled: bool,
    last_flush: Option<Instant>,
}

#[derive(Clone, Default)]
struct VideoUiDispatcher {
    pending: Arc<Mutex<PendingVideoUi>>,
}

/// One isolated Nexo application instance: its own window, identity, store and
/// network discovery. Dropping the instance shuts the network loop down and
/// releases the local database.
pub struct AppInstance {
    pub window: AppWindow,
    pub tray: Option<NexoTray>,
    _refresh_timer: slint::Timer,
    network_peer_id: Arc<Mutex<Option<String>>>,
    active_relay_peer_id: Arc<Mutex<Option<String>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    discovery_thread: Option<thread::JoinHandle<()>>,
}

impl AppInstance {
    /// Signal the background discovery loop to stop and release its resources.
    /// The instance also performs this shutdown when dropped.
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.discovery_thread.take() {
            let _ = thread.join();
        }
    }

    /// Returns the authenticated libp2p identity used by this isolated instance.
    /// This is primarily useful to integration tests and diagnostics; the UI
    /// continues to display the device identity separately.
    #[must_use]
    pub fn network_peer_id(&self) -> Option<String> {
        self.network_peer_id.lock().ok().and_then(|id| id.clone())
    }

    /// Returns the relay currently selected for the active call, when any.
    /// Kept outside the UI text so integration tests and diagnostics can
    /// observe topology changes without racing presentation updates.
    #[must_use]
    pub fn active_relay_peer_id(&self) -> Option<String> {
        self.active_relay_peer_id
            .lock()
            .ok()
            .and_then(|id| id.clone())
    }
}

impl Drop for AppInstance {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::struct_excessive_bools)]
struct AppState {
    identity: DeviceIdentity,
    store: LocalStore,
    selected: Option<Community>,
    selected_channel: Option<Channel>,
    listen_addresses: Arc<Mutex<Vec<String>>>,
    dial_queue: tokio::sync::mpsc::UnboundedSender<String>,
    call_queue: tokio::sync::mpsc::UnboundedSender<CallCommand>,
    direct_queue: tokio::sync::mpsc::UnboundedSender<DirectCommand>,
    moderation_queue: tokio::sync::mpsc::UnboundedSender<ModerationCommand>,
    file_queue: tokio::sync::mpsc::UnboundedSender<FileCommand>,
    voice_queue: tokio::sync::mpsc::UnboundedSender<VoiceCommand>,
    call_muted: bool,
    input_devices: Vec<nexo_media::AudioDeviceInfo>,
    output_devices: Vec<nexo_media::AudioDeviceInfo>,
    video_devices: Vec<nexo_video::VideoDeviceInfo>,
    selected_input: Option<String>,
    selected_output: Option<String>,
    selected_video: Option<String>,
    participants: Arc<Mutex<Vec<nexo_media::ParticipantStatus>>>,
    selected_direct_peer: Option<[u8; 32]>,
}

enum CallCommand {
    Join {
        community_id: uuid::Uuid,
        call_id: uuid::Uuid,
        input_device: Option<String>,
        output_device: Option<String>,
        video_device: Option<String>,
    },
    SelectInput(String),
    SelectOutput(String),
    SelectVideo(String),
    SetMuted(bool),
    SetVideoEnabled(bool),
    SetScreenSharing(bool),
    Leave,
}

enum DirectCommand {
    Send {
        community_id: uuid::Uuid,
        recipient_key: [u8; 32],
        body: String,
    },
}

enum ModerationCommand {
    RevokeMember {
        community_id: uuid::Uuid,
        member_key: [u8; 32],
    },
}

enum FileCommand {
    Send {
        community_id: uuid::Uuid,
        channel_id: uuid::Uuid,
        path: PathBuf,
    },
}

/// Signed file metadata and chunks carried over the authenticated WebRTC
/// data channel. The transport limits each serialized message before it is
/// handed to this layer.
#[derive(Deserialize, Serialize)]
enum WebRtcFileMessage {
    Offer(FileTransferOffer),
    Chunk {
        transfer_id: uuid::Uuid,
        chunk_index: u32,
        data_base64: String,
        chunk_sha256: [u8; 32],
    },
}

/// The SDP is carried with the exact codec selected by the offerer. Keeping
/// this decision in the same signed call signal avoids a race with the
/// separate capabilities signal on high-latency links.
#[derive(Deserialize, Serialize)]
struct CallOfferPayload {
    codec: String,
    sdp: String,
}

fn encode_call_offer(codec: VideoCodec, sdp: String) -> Result<String> {
    let codec = match codec {
        VideoCodec::Vp8 => "vp8",
        VideoCodec::H264 => "h264",
    };
    Ok(serde_json::to_string(&CallOfferPayload {
        codec: codec.to_owned(),
        sdp,
    })?)
}

fn decode_call_offer(payload: &str) -> Result<(Option<VideoCodec>, String)> {
    // Accept pre-wrapper offers generated by older local builds. Their codec
    // remains selected by the authenticated capability exchange.
    let Ok(wrapper) = serde_json::from_str::<CallOfferPayload>(payload) else {
        return Ok((None, payload.to_owned()));
    };
    let codec = match wrapper.codec.as_str() {
        "vp8" => VideoCodec::Vp8,
        "h264" => VideoCodec::H264,
        _ => anyhow::bail!("codec de oferta WebRTC desconhecido"),
    };
    if wrapper.sdp.trim().is_empty() {
        anyhow::bail!("oferta WebRTC sem SDP");
    }
    Ok((Some(codec), wrapper.sdp))
}

enum VoiceCommand {
    Start {
        community_id: uuid::Uuid,
        channel_id: uuid::Uuid,
        input_device: Option<String>,
    },
    Stop,
}

struct VoiceRecorder {
    source: nexo_media::InputFrameSource,
    dsp: nexo_media::AudioDspPipeline,
    samples: Vec<f32>,
}

impl VoiceRecorder {
    const MAX_SAMPLES: usize = nexo_media::OPUS_SAMPLE_RATE as usize * 60;

    fn start(input_device: Option<&str>) -> Result<Self> {
        let source = match input_device {
            Some(device) => nexo_media::InputFrameSource::start_input(device),
            None => nexo_media::InputFrameSource::start_default(),
        }
        .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        Ok(Self {
            source,
            dsp: nexo_media::AudioDspPipeline::new(),
            samples: Vec::with_capacity(Self::MAX_SAMPLES),
        })
    }

    fn poll(&mut self) -> Result<()> {
        while let Some(frame) = self
            .source
            .try_frame()
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
        {
            let remaining = Self::MAX_SAMPLES.saturating_sub(self.samples.len());
            let mut samples = frame.samples;
            self.dsp.process_input_frame(&mut samples, None);
            self.samples.extend(samples.into_iter().take(remaining));
            if self.samples.len() >= Self::MAX_SAMPLES {
                break;
            }
        }
        Ok(())
    }

    fn finish(self, directory: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("voice-{}.wav", uuid::Uuid::new_v4()));
        write_pcm_wav(&path, &self.samples)?;
        Ok(path)
    }
}

fn write_pcm_wav(path: &Path, samples: &[f32]) -> Result<()> {
    let data_bytes = samples
        .len()
        .checked_mul(2)
        .context("nota de voz excede o tamanho suportado")?;
    let data_len = u32::try_from(data_bytes).context("nota de voz excede o formato WAV")?;
    let riff_len = 36_u32
        .checked_add(data_len)
        .context("cabecalho WAV excede o limite")?;
    let mut file = std::fs::File::create(path)?;
    file.write_all(b"RIFF")?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16_u32.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&nexo_media::OPUS_SAMPLE_RATE.to_le_bytes())?;
    file.write_all(&(nexo_media::OPUS_SAMPLE_RATE * 2).to_le_bytes())?;
    file.write_all(&2_u16.to_le_bytes())?;
    file.write_all(&16_u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    for &sample in samples {
        #[allow(clippy::cast_possible_truncation)]
        let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        file.write_all(&value.to_le_bytes())?;
    }
    file.sync_all()?;
    Ok(())
}

struct IncomingFile {
    peer_id: libp2p::PeerId,
    path: PathBuf,
    total_chunks: u32,
    next_chunk: u32,
}

const MAX_FILE_BYTES: u64 = 256 * 1024 * 1024;
const DEFAULT_FILE_CHUNK_SIZE: u32 = nexo_core::DEFAULT_CHUNK_SIZE;

/// Build a full application instance rooted at `data_dir`. The directory holds
/// the persisted identity key and the `SQLite` store.
pub fn start_app(data_dir: &Path) -> Result<AppInstance> {
    start_app_with_camera_discovery(data_dir, true)
}

/// Build an application instance without opening or enumerating a camera.
///
/// This is used by integration scenarios that create several app instances in
/// one process. The normal desktop entry point keeps camera discovery enabled.
#[doc(hidden)]
pub fn start_app_without_camera(data_dir: &Path) -> Result<AppInstance> {
    start_app_with_camera_discovery(data_dir, false)
}

#[allow(clippy::too_many_lines)]
fn start_app_with_camera_discovery(data_dir: &Path, discover_camera: bool) -> Result<AppInstance> {
    let window = AppWindow::new()?;
    let identity = DeviceIdentity::load_or_create(&data_dir.join("identity.key"))?;
    let store = LocalStore::open(&data_dir.join("nexo.sqlite3"))?;
    restore_local_authorizations(&store, &identity)?;
    let selected = store.communities()?.into_iter().next();
    let selected_channel = selected.as_ref().and_then(|community| {
        store
            .channels(community.id)
            .ok()?
            .into_iter()
            .find(|channel| channel.id == community.default_channel_id)
    });
    let listen_addresses = Arc::new(Mutex::new(Vec::new()));
    let network_peer_id = Arc::new(Mutex::new(None));
    let active_relay_peer_id = Arc::new(Mutex::new(None));
    let (dial_queue, dial_requests) = tokio::sync::mpsc::unbounded_channel();
    let (call_queue, call_requests) = tokio::sync::mpsc::unbounded_channel();
    let (direct_queue, direct_requests) = tokio::sync::mpsc::unbounded_channel();
    let (moderation_queue, moderation_requests) = tokio::sync::mpsc::unbounded_channel();
    let (file_queue, file_requests) = tokio::sync::mpsc::unbounded_channel();
    let (voice_queue, voice_requests) = tokio::sync::mpsc::unbounded_channel();
    let (input_devices, output_devices) =
        split_audio_devices(nexo_media::enumerate_audio_devices().unwrap_or_default());
    let video_devices = if discover_camera {
        nexo_video::enumerate_cameras().unwrap_or_default()
    } else {
        Vec::new()
    };
    let selected_input = default_device_id(&input_devices);
    let selected_output = default_device_id(&output_devices);
    let selected_video = video_devices.first().map(|d| d.id.clone());
    let participants = Arc::new(Mutex::new(Vec::new()));
    let state = Rc::new(RefCell::new(AppState {
        identity: identity.clone(),
        store,
        selected,
        selected_channel,
        listen_addresses: Arc::clone(&listen_addresses),
        dial_queue,
        call_queue,
        direct_queue,
        moderation_queue,
        file_queue,
        voice_queue,
        call_muted: false,
        input_devices,
        output_devices,
        video_devices,
        selected_input,
        selected_output,
        selected_video,
        participants: Arc::clone(&participants),
        selected_direct_peer: None,
    }));

    window.set_peer_id(format!("Dispositivo {}", short_id(&identity.public_key_text())).into());
    bind_actions(&window, &state);
    refresh_device_catalog(&window, &state.borrow());
    refresh_view(&window, &state.borrow())?;
    let refresh_timer = start_view_refresh(&window, Rc::clone(&state));
    let video_ui = VideoUiDispatcher::default();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let discovery_thread = start_discovery(
        window.as_weak(),
        identity,
        data_dir.join("nexo.sqlite3"),
        listen_addresses,
        Arc::clone(&network_peer_id),
        Arc::clone(&active_relay_peer_id),
        dial_requests,
        call_requests,
        direct_requests,
        moderation_requests,
        file_requests,
        voice_requests,
        participants,
        video_ui,
        shutdown_rx,
    );
    let tray = NexoTray::new().ok();
    if let Some(tray) = tray.as_ref() {
        let weak_window = window.as_weak();
        tray.on_show_window(move || {
            if let Some(window) = weak_window.upgrade() {
                let _ = window.show();
            }
        });
        tray.on_quit_app(|| {
            let _ = slint::quit_event_loop();
        });
        let _ = tray.show();
    }
    if tray.is_some() {
        window
            .window()
            .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    }
    Ok(AppInstance {
        window,
        tray,
        _refresh_timer: refresh_timer,
        network_peer_id,
        active_relay_peer_id,
        shutdown: Some(shutdown_tx),
        discovery_thread: Some(discovery_thread),
    })
}

fn restore_local_authorizations(store: &LocalStore, identity: &DeviceIdentity) -> Result<()> {
    let own_key = identity.public_key_bytes();
    for community in store.communities()? {
        if !store.is_revoked_member(community.id, &own_key)? {
            store.authorize_member(community.id, &own_key, current_timestamp())?;
        }
    }
    Ok(())
}

/// Run the desktop application for the default data directory, blocking until
/// the window is closed.
pub fn run() -> Result<()> {
    let data_dir = data_dir()?;
    install_panic_log(&data_dir);
    let mut app = start_app(&data_dir)?;
    let result = app.window.run();
    app.shutdown();
    result.map_err(Into::into)
}

fn install_panic_log(data_dir: &Path) {
    let log_path = data_dir.join("crash.log");
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            use std::io::Write as _;
            let _ = writeln!(
                file,
                "\n=== {} ===\n{}",
                chrono::Local::now().to_rfc3339(),
                panic_info
            );
        }
        previous(panic_info);
    }));
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NEXO_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::data_local_dir().context("the operating system has no local data folder")?;
    Ok(base.join("Nexo"))
}

#[allow(clippy::too_many_lines)]
fn bind_actions(app: &AppWindow, state: &Rc<RefCell<AppState>>) {
    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_create_network(move |name| {
        let result = create_community(&action_state, name.trim());
        if let Some(app) = weak.upgrade() {
            match result {
                Ok(code) => {
                    app.set_invite_code(code.into());
                    set_result_status(&app, "Comunidade criada. Convite pronto para compartilhar");
                    refresh_or_report(&app, &action_state.borrow());
                }
                Err(error) => set_result_status(&app, &format!("Nao foi possivel criar: {error}")),
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_join_network(move |code| {
        let result = join_community(&action_state, code.trim());
        if let Some(app) = weak.upgrade() {
            match result {
                Ok(()) => {
                    set_result_status(&app, "Convite aceito. Comunidade salva neste dispositivo");
                    refresh_or_report(&app, &action_state.borrow());
                }
                Err(error) => set_result_status(&app, &format!("Convite recusado: {error}")),
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_community(move |id| {
        let parsed = uuid::Uuid::parse_str(id.trim());
        if let (Ok(id), Some(app)) = (parsed, weak.upgrade()) {
            let mut state = action_state.borrow_mut();
            if let Ok(communities) = state.store.communities() {
                state.selected = communities.into_iter().find(|community| community.id == id);
                state.selected_direct_peer = None;
                state.selected_channel = state.selected.as_ref().and_then(|community| {
                    state
                        .store
                        .channels(community.id)
                        .ok()?
                        .into_iter()
                        .find(|channel| channel.id == community.default_channel_id)
                });
                refresh_or_report(&app, &state);
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_channel(move |id| {
        let Ok(channel_id) = uuid::Uuid::parse_str(id.trim()) else {
            return;
        };
        let mut state = action_state.borrow_mut();
        let Some(community_id) = state.selected.as_ref().map(|community| community.id) else {
            return;
        };
        state.selected_channel = state
            .store
            .channels(community_id)
            .ok()
            .and_then(|channels| {
                channels
                    .into_iter()
                    .find(|channel| channel.id == channel_id)
            });
        if let Some(app) = weak.upgrade() {
            refresh_or_report(&app, &state);
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_create_channel(move |name, kind| {
        let mut state = action_state.borrow_mut();
        let Some(community_id) = state.selected.as_ref().map(|community| community.id) else {
            return;
        };
        let channel_kind = if kind.eq_ignore_ascii_case("voice") {
            ChannelKind::Voice
        } else {
            ChannelKind::Text
        };
        match state
            .store
            .create_channel(community_id, name.as_str(), channel_kind)
        {
            Ok(channel) => {
                state.selected_channel = Some(channel);
                if let Some(app) = weak.upgrade() {
                    refresh_or_report(&app, &state);
                }
            }
            Err(error) => {
                if let Some(app) = weak.upgrade() {
                    set_result_status(&app, &format!("Canal nao criado: {error}"));
                }
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_send_message(move |body| {
        let result = send_message(&action_state, body.trim());
        if let Some(app) = weak.upgrade() {
            match result {
                Ok(()) => {
                    refresh_or_report(&app, &action_state.borrow());
                    true
                }
                Err(error) => {
                    set_result_status(&app, &format!("Mensagem nao enviada: {error}"));
                    false
                }
            }
        } else {
            false
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_direct_peer(move |value| {
        let Ok(key) = DeviceIdentity::decode_public_key_text(value.trim()) else {
            return;
        };
        let mut state = action_state.borrow_mut();
        if state.selected.as_ref().is_some_and(|community| {
            state
                .store
                .is_authorized_member(community.id, &key)
                .unwrap_or(false)
        }) {
            state.selected_direct_peer = Some(key);
            if let Some(app) = weak.upgrade() {
                refresh_or_report(&app, &state);
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_revoke_member(move |value| {
        let Ok(member_key) = DeviceIdentity::decode_public_key_text(value.trim()) else {
            if let Some(app) = weak.upgrade() {
                set_result_status(&app, "Membro invalido");
            }
            return;
        };
        let state = action_state.borrow();
        let Some(community_id) = state.selected.as_ref().map(|community| community.id) else {
            return;
        };
        let own_key = state.identity.public_key_bytes();
        let can_manage = is_local_founder(&state.store, community_id, &own_key).unwrap_or(false);
        if !can_manage || member_key == own_key {
            if let Some(app) = weak.upgrade() {
                set_result_status(&app, "Somente o fundador pode remover outro membro");
            }
            return;
        }
        if !state
            .store
            .is_authorized_member(community_id, &member_key)
            .unwrap_or(false)
        {
            if let Some(app) = weak.upgrade() {
                set_result_status(&app, "Este membro ja nao esta autorizado");
            }
            return;
        }
        if state
            .moderation_queue
            .send(ModerationCommand::RevokeMember {
                community_id,
                member_key,
            })
            .is_ok()
        {
            if let Some(app) = weak.upgrade() {
                set_result_status(&app, "Preparando remocao do membro...");
            }
        } else if let Some(app) = weak.upgrade() {
            set_result_status(&app, "O servico de moderacao nao esta disponivel");
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_send_direct_message(move |body| {
        let body = body.trim();
        if body.is_empty() {
            return false;
        }
        let state = action_state.borrow();
        let Some(community_id) = state.selected.as_ref().map(|community| community.id) else {
            return false;
        };
        let Some(recipient_key) = state.selected_direct_peer else {
            if let Some(app) = weak.upgrade() {
                set_result_status(&app, "Selecione uma mensagem direta primeiro");
            }
            return false;
        };
        let result = state.direct_queue.send(DirectCommand::Send {
            community_id,
            recipient_key,
            body: body.to_owned(),
        });
        if let Some(app) = weak.upgrade() {
            if result.is_ok() {
                set_result_status(&app, "Enviando mensagem direta...");
                true
            } else {
                set_result_status(&app, "O serviço de mensagens diretas não está disponível");
                false
            }
        } else {
            false
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_attach_file(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let state = action_state.borrow();
        let Some(community_id) = state.selected.as_ref().map(|c| c.id) else {
            set_result_status(&app, "Selecione uma comunidade primeiro");
            return;
        };
        let channel_id = state.selected_channel.as_ref().map_or_else(
            || {
                state
                    .selected
                    .as_ref()
                    .map_or(community_id, |c| c.default_channel_id)
            },
            |channel| channel.id,
        );
        let file_queue = state.file_queue.clone();
        drop(state);
        set_result_status(&app, "Abrindo seletor de arquivo...");
        let weak = app.as_weak();
        thread::spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Enviar arquivo no Nexo")
                .pick_file();
            let _ = slint::invoke_from_event_loop(move || {
                let Some(app) = weak.upgrade() else {
                    return;
                };
                let Some(path) = path else {
                    set_result_status(&app, "Selecao de arquivo cancelada");
                    return;
                };
                if file_queue
                    .send(FileCommand::Send {
                        community_id,
                        channel_id,
                        path,
                    })
                    .is_ok()
                {
                    set_result_status(&app, "Preparando arquivo para envio...");
                } else {
                    set_result_status(&app, "O serviço de arquivos não está disponível");
                }
            });
        });
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_toggle_voice_recording(move || {
        let Some(app) = weak.upgrade() else {
            return;
        };
        let is_recording = app.get_is_recording_voice();
        let state = action_state.borrow();
        let command = if is_recording {
            VoiceCommand::Stop
        } else {
            let Some(community_id) = state.selected.as_ref().map(|community| community.id) else {
                set_result_status(&app, "Selecione uma comunidade primeiro");
                return;
            };
            let channel_id = state.selected_channel.as_ref().map_or_else(
                || {
                    state
                        .selected
                        .as_ref()
                        .map_or(community_id, |c| c.default_channel_id)
                },
                |channel| channel.id,
            );
            VoiceCommand::Start {
                community_id,
                channel_id,
                input_device: state.selected_input.clone(),
            }
        };
        if state.voice_queue.send(command).is_ok() {
            app.set_is_recording_voice(!is_recording);
            set_result_status(
                &app,
                if is_recording {
                    "Finalizando nota de voz..."
                } else {
                    "Gravando nota de voz..."
                },
            );
        } else {
            set_result_status(&app, "O serviço de áudio não está disponível");
        }
    });

    bind_call_actions(app, state);
}

#[allow(clippy::too_many_lines)]
fn bind_call_actions(app: &AppWindow, state: &Rc<RefCell<AppState>>) {
    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_join_call(move || {
        let mut state = action_state.borrow_mut();
        let Some(app) = weak.upgrade() else {
            return;
        };
        if app.get_call_active() || app.get_call_starting() {
            return;
        }
        let Some(community) = state.selected.as_ref() else {
            set_result_status(&app, "Selecione ou crie uma comunidade primeiro");
            return;
        };
        let call_id = state
            .selected_channel
            .as_ref()
            .filter(|channel| channel.kind == ChannelKind::Voice)
            .map_or(community.default_channel_id, |channel| channel.id);
        let command = CallCommand::Join {
            community_id: community.id,
            call_id,
            input_device: state.selected_input.clone(),
            output_device: state.selected_output.clone(),
            video_device: state.selected_video.clone(),
        };
        if state.call_queue.send(command).is_ok() {
            state.call_muted = false;
            app.set_call_starting(true);
            app.set_call_muted(false);
            app.set_call_status("Conectando".into());
        } else {
            set_result_status(&app, "O serviço de chamadas não está disponível");
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_set_muted(move |muted| {
        let mut state = action_state.borrow_mut();
        if state.call_queue.send(CallCommand::SetMuted(muted)).is_ok() {
            state.call_muted = muted;
            if let Some(app) = weak.upgrade() {
                app.set_call_muted(muted);
            }
        } else if let Some(app) = weak.upgrade() {
            set_result_status(&app, "Não foi possível alterar o microfone");
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_leave_call(move || {
        let mut state = action_state.borrow_mut();
        if state.call_queue.send(CallCommand::Leave).is_ok() {
            state.call_muted = false;
            if let Some(app) = weak.upgrade() {
                app.set_call_active(false);
                app.set_call_starting(false);
                app.set_call_muted(false);
                app.set_call_status("Fora da voz".into());
            }
        } else if let Some(app) = weak.upgrade() {
            set_result_status(&app, "O serviço de chamadas não está disponível");
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_input_device(move |display_name| {
        let mut state = action_state.borrow_mut();
        let id = id_for_display(&state.input_devices, display_name.as_str())
            .or_else(|| Some(display_name.to_string()));
        if weak.upgrade().is_some_and(|app| app.get_call_active()) {
            let _ = state
                .call_queue
                .send(CallCommand::SelectInput(id.clone().unwrap_or_default()));
        }
        if let Some(app) = weak.upgrade() {
            app.set_selected_input_device(device_label(&state.input_devices, id.as_deref()).into());
        }
        let _ = state
            .store
            .set_metadata("pref_input_device", display_name.as_str());
        state.selected_input = id;
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_output_device(move |display_name| {
        let mut state = action_state.borrow_mut();
        let id = id_for_display(&state.output_devices, display_name.as_str())
            .or_else(|| Some(display_name.to_string()));
        if weak.upgrade().is_some_and(|app| app.get_call_active()) {
            let _ = state
                .call_queue
                .send(CallCommand::SelectOutput(id.clone().unwrap_or_default()));
        }
        if let Some(app) = weak.upgrade() {
            app.set_selected_output_device(
                device_label(&state.output_devices, id.as_deref()).into(),
            );
        }
        let _ = state
            .store
            .set_metadata("pref_output_device", display_name.as_str());
        state.selected_output = id;
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_video_device(move |display_name| {
        let mut state = action_state.borrow_mut();
        let id = video_id_for_display(&state.video_devices, display_name.as_str())
            .or_else(|| Some(display_name.to_string()));
        if weak.upgrade().is_some_and(|app| app.get_call_active()) {
            let _ = state
                .call_queue
                .send(CallCommand::SelectVideo(id.clone().unwrap_or_default()));
        }
        if let Some(app) = weak.upgrade() {
            app.set_selected_video_device(
                video_device_label(&state.video_devices, id.as_deref()).into(),
            );
        }
        let _ = state
            .store
            .set_metadata("pref_video_device", display_name.as_str());
        state.selected_video = id;
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_toggle_video(move || {
        let state = action_state.borrow();
        let Some(app) = weak.upgrade() else {
            return;
        };
        let enabled = !app.get_video_enabled();
        if state
            .call_queue
            .send(CallCommand::SetVideoEnabled(enabled))
            .is_ok()
        {
            app.set_video_enabled(enabled);
            if !enabled {
                app.set_has_local_video(false);
            }
        } else {
            set_result_status(&app, "O serviço de chamadas não está disponível");
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_toggle_screen_share(move || {
        let state = action_state.borrow();
        let Some(app) = weak.upgrade() else {
            return;
        };
        let sharing = !app.get_screen_sharing();
        if state
            .call_queue
            .send(CallCommand::SetScreenSharing(sharing))
            .is_ok()
        {
            app.set_screen_sharing(sharing);
            if !sharing && !app.get_video_enabled() {
                app.set_has_local_video(false);
            }
        } else {
            set_result_status(&app, "O serviço de chamadas não está disponível");
        }
    });
}

fn create_community(state: &Rc<RefCell<AppState>>, name: &str) -> Result<String> {
    let now = current_timestamp();
    let mut state = state.borrow_mut();
    let addresses = state
        .listen_addresses
        .lock()
        .map_err(|_| anyhow::anyhow!("enderecos locais indisponiveis"))?
        .clone();
    if addresses.is_empty() {
        anyhow::bail!("a rede local ainda esta iniciando; tente novamente em um instante");
    }
    let invite = NetworkInvite::create(
        &state.identity,
        name.to_owned(),
        addresses,
        now,
        24 * 60 * 60,
    )?;
    let community = state
        .store
        .create_community(invite.network_id, &invite.network_name, now)?;
    let credential = CommunityCredential::claim(&state.identity, invite.clone(), now)?;
    state
        .store
        .authorize_member(community.id, &state.identity.public_key_bytes(), now)?;
    state.store.save_credential(&credential)?;
    let founder_key = state.identity.public_key_bytes();
    if let Some(group_secret) = invite.group_secret_bytes() {
        state.store.ensure_mls_group_with_secret(
            community.id,
            mls_device_id(&founder_key),
            founder_key,
            &[founder_key],
            group_secret,
        )?;
    } else {
        state.store.ensure_mls_group(
            community.id,
            mls_device_id(&founder_key),
            founder_key,
            &[founder_key],
        )?;
    }
    for address in &invite.addresses {
        let _ = state.dial_queue.send(address.clone());
    }
    state.selected = Some(community);
    state.selected_channel = state.selected.as_ref().and_then(|community| {
        state
            .store
            .channels(community.id)
            .ok()?
            .into_iter()
            .find(|channel| channel.id == community.default_channel_id)
    });
    Ok(invite.encode()?)
}

fn join_community(state: &Rc<RefCell<AppState>>, code: &str) -> Result<()> {
    let now = current_timestamp();
    let invite = NetworkInvite::decode_and_verify(code, now)?;
    let mut state = state.borrow_mut();
    let community = state
        .store
        .create_community(invite.network_id, &invite.network_name, now)?;
    let credential = CommunityCredential::claim(&state.identity, invite.clone(), now)?;
    let inviter_key = DeviceIdentity::decode_public_key_text(&invite.inviter_key)?;
    if let Some(group_secret) = invite.group_secret_bytes() {
        state.store.ensure_mls_group_with_secret(
            community.id,
            mls_device_id(&inviter_key),
            inviter_key,
            &[inviter_key],
            group_secret,
        )?;
    } else {
        state.store.ensure_mls_group(
            community.id,
            mls_device_id(&inviter_key),
            inviter_key,
            &[inviter_key],
        )?;
    }
    state
        .store
        .authorize_member(community.id, &inviter_key, invite.created_at)?;
    let member_key = state.identity.public_key_bytes();
    state
        .store
        .authorize_member(community.id, &member_key, now)?;
    state.store.save_credential(&credential)?;
    let mut mls_state = state
        .store
        .mls_group(community.id)?
        .context("MLS group state missing after initialization")?;
    let commit = MlsCommit::create_add(
        &state.identity,
        &mls_state,
        mls_device_id(&member_key),
        member_key,
    )?;
    mls_state.apply_join_commit(&commit)?;
    state.store.save_mls_commit(&commit)?;
    state.store.save_mls_group(&mls_state)?;
    for address in &invite.addresses {
        let _ = state.dial_queue.send(address.clone());
    }
    state.selected = Some(community);
    state.selected_channel = state.selected.as_ref().and_then(|community| {
        state
            .store
            .channels(community.id)
            .ok()?
            .into_iter()
            .find(|channel| channel.id == community.default_channel_id)
    });
    Ok(())
}

fn send_message(state: &Rc<RefCell<AppState>>, body: &str) -> Result<()> {
    let state = state.borrow();
    let community = state
        .selected
        .as_ref()
        .context("nenhuma comunidade selecionada")?;
    let channel_id = state
        .selected_channel
        .as_ref()
        .map_or(community.default_channel_id, |channel| channel.id);
    let now = current_timestamp();
    let processed_body = nexo_core::replace_emoji_shortcodes(body);
    let mls_state = ensure_local_mls_state(&state.store, community.id)?;
    let message = SignedMessage::create_encrypted(
        &state.identity,
        community.id,
        channel_id,
        &processed_body,
        now,
        &mls_state,
    )?;
    state
        .store
        .insert_message_with_mls(&message, Some(&mls_state), now)?;
    Ok(())
}

fn is_local_founder(
    store: &LocalStore,
    community_id: uuid::Uuid,
    own_key: &[u8; 32],
) -> Result<bool> {
    Ok(store
        .credentials(community_id)?
        .into_iter()
        .any(|credential| {
            DeviceIdentity::decode_public_key_text(&credential.invite.inviter_key)
                .is_ok_and(|founder_key| founder_key == *own_key)
        }))
}

#[allow(clippy::too_many_lines)]
fn refresh_view(app: &AppWindow, state: &AppState) -> Result<()> {
    let communities = state.store.communities()?;
    let community_rows = communities
        .iter()
        .map(|community| CommunityRow {
            id: community.id.to_string().into(),
            name: community.name.clone().into(),
            initial: community
                .name
                .chars()
                .next()
                .unwrap_or('N')
                .to_uppercase()
                .to_string()
                .into(),
        })
        .collect::<Vec<_>>();
    app.set_communities(ModelRc::new(VecModel::from(community_rows)));

    if let Some(community) = &state.selected {
        app.set_has_community(true);
        app.set_active_community(community.name.clone().into());
        let channels = state.store.channels(community.id)?;
        let selected_channel_id = state
            .selected_channel
            .as_ref()
            .map_or(community.default_channel_id, |channel| channel.id);
        let active_channel = channels
            .iter()
            .find(|channel| channel.id == selected_channel_id)
            .or_else(|| channels.first());
        app.set_active_channel(
            active_channel
                .map_or_else(|| "geral".to_owned(), |channel| channel.name.clone())
                .into(),
        );
        app.set_channels(ModelRc::new(VecModel::from(
            channels
                .iter()
                .map(|channel| ChannelRow {
                    id: channel.id.to_string().into(),
                    name: channel.name.clone().into(),
                    kind: channel.kind.as_str().into(),
                    selected: channel.id == selected_channel_id,
                })
                .collect::<Vec<_>>(),
        )));
        let own_key = state.identity.public_key_bytes();
        let can_manage_members = is_local_founder(&state.store, community.id, &own_key)?;
        let member_rows = state
            .store
            .authorized_members(community.id)?
            .into_iter()
            .map(|key| MemberRow {
                id: URL_SAFE_NO_PAD.encode(key).into(),
                label: if key == own_key {
                    "Voce (fundador)".into()
                } else {
                    format!("Pessoa {}", short_id(&hex_prefix(&key))).into()
                },
                can_remove: can_manage_members && key != own_key,
                is_self: key == own_key,
            })
            .collect::<Vec<_>>();
        app.set_can_manage_members(can_manage_members);
        app.set_members(ModelRc::new(VecModel::from(member_rows)));
        let direct_peers = state
            .store
            .authorized_members(community.id)?
            .into_iter()
            .filter(|key| *key != own_key)
            .map(|key| DirectPeerRow {
                id: URL_SAFE_NO_PAD.encode(key).into(),
                label: format!("Pessoa {}", short_id(&hex_prefix(&key))).into(),
                selected: state.selected_direct_peer == Some(key),
            })
            .collect::<Vec<_>>();
        app.set_direct_peers(ModelRc::new(VecModel::from(direct_peers)));
        let active_direct_key = state.selected_direct_peer.filter(|key| {
            state
                .store
                .is_authorized_member(community.id, key)
                .unwrap_or(false)
                && *key != own_key
        });
        app.set_active_direct_peer(
            active_direct_key
                .map_or_else(SharedString::new, |key| URL_SAFE_NO_PAD.encode(key).into()),
        );
        let direct_rows = if let Some(peer_key) = active_direct_key {
            let conversation_id = direct_conversation_id(community.id, own_key, peer_key);
            state
                .store
                .direct_messages(conversation_id, HISTORY_LIMIT, current_timestamp())?
                .into_iter()
                .map(|message| MessageRow {
                    author: if message.envelope.sender_key == own_key {
                        SharedString::from("Voce")
                    } else {
                        format!(
                            "Pessoa {}",
                            short_id(&hex_prefix(&message.envelope.sender_key))
                        )
                        .into()
                    },
                    body: message.body.into(),
                    time: format_time(message.envelope.created_at).into(),
                    mine: message.envelope.sender_key == own_key,
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        app.set_direct_messages(ModelRc::new(VecModel::from(direct_rows)));
        let mls_state = ensure_local_mls_state(&state.store, community.id)?;
        let rows = state
            .store
            .messages(
                active_channel.map_or(community.default_channel_id, |channel| channel.id),
                HISTORY_LIMIT,
                current_timestamp(),
            )?
            .into_iter()
            .map(|message| MessageRow {
                author: if message.author_key == own_key {
                    SharedString::from("Voce")
                } else {
                    format!("Pessoa {}", short_id(&hex_prefix(&message.author_key))).into()
                },
                body: message
                    .decrypt_body(&mls_state)
                    .unwrap_or_else(|_| "[mensagem indisponivel]".to_owned())
                    .into(),
                time: format_time(message.created_at).into(),
                mine: message.author_key == own_key,
            })
            .collect::<Vec<_>>();
        app.set_messages(ModelRc::new(VecModel::from(rows)));
    } else {
        app.set_has_community(false);
        app.set_active_community(SharedString::new());
        app.set_active_channel(SharedString::new());
        app.set_channels(ModelRc::new(VecModel::<ChannelRow>::default()));
        app.set_direct_peers(ModelRc::new(VecModel::<DirectPeerRow>::default()));
        app.set_members(ModelRc::new(VecModel::<MemberRow>::default()));
        app.set_can_manage_members(false);
        app.set_active_direct_peer(SharedString::new());
        app.set_direct_messages(ModelRc::new(VecModel::<MessageRow>::default()));
        app.set_messages(ModelRc::new(VecModel::<MessageRow>::default()));
    }

    let participant_rows = state
        .participants
        .lock()
        .map(|participants| participants.clone())
        .unwrap_or_default()
        .into_iter()
        .map(|participant| ParticipantRow {
            peer_id: participant.peer_id.clone().into(),
            short_id: format!("Pessoa {}", short_id(&participant.peer_id)).into(),
            connected: participant.connected,
        })
        .collect::<Vec<_>>();
    app.set_participants(ModelRc::new(VecModel::from(participant_rows)));
    Ok(())
}

fn device_label(devices: &[nexo_media::AudioDeviceInfo], id: Option<&str>) -> String {
    match id {
        Some(id) => devices
            .iter()
            .find(|device| device.id == id)
            .map_or_else(|| id.to_owned(), |device| device.name.clone()),
        None => devices.iter().find(|device| device.is_default).map_or_else(
            || "Padrao do sistema".to_owned(),
            |device| device.name.clone(),
        ),
    }
}

fn refresh_device_catalog(app: &AppWindow, state: &AppState) {
    app.set_input_device_names(ModelRc::new(VecModel::from(
        display_rows(&state.input_devices)
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )));
    app.set_output_device_names(ModelRc::new(VecModel::from(
        display_rows(&state.output_devices)
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )));
    app.set_video_device_names(ModelRc::new(VecModel::from(
        video_display_rows(&state.video_devices)
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )));
    app.set_selected_input_device(
        device_label(&state.input_devices, state.selected_input.as_deref()).into(),
    );
    app.set_selected_output_device(
        device_label(&state.output_devices, state.selected_output.as_deref()).into(),
    );
    app.set_selected_video_device(
        video_device_label(&state.video_devices, state.selected_video.as_deref()).into(),
    );
}

fn video_device_label(devices: &[nexo_video::VideoDeviceInfo], id: Option<&str>) -> String {
    match id {
        Some(id) => devices
            .iter()
            .find(|device| device.id == id)
            .map_or_else(|| id.to_owned(), |device| device.name.clone()),
        None => devices.first().map_or_else(
            || "Padrao / Sintetico".to_owned(),
            |device| device.name.clone(),
        ),
    }
}

fn video_display_rows(devices: &[nexo_video::VideoDeviceInfo]) -> Vec<String> {
    let mut used = std::collections::HashMap::<String, usize>::new();
    devices
        .iter()
        .map(|device| {
            let index = used.entry(device.name.clone()).or_insert(0);
            let text = if *index == 0 {
                device.name.clone()
            } else {
                format!("{} ({index})", device.name)
            };
            *index += 1;
            text
        })
        .collect()
}

fn video_id_for_display(devices: &[nexo_video::VideoDeviceInfo], display: &str) -> Option<String> {
    video_display_rows(devices)
        .iter()
        .position(|row| row == display)
        .map(|index| devices[index].id.clone())
}

fn split_audio_devices(
    devices: Vec<nexo_media::AudioDeviceInfo>,
) -> (
    Vec<nexo_media::AudioDeviceInfo>,
    Vec<nexo_media::AudioDeviceInfo>,
) {
    let mut input = Vec::new();
    let mut output = Vec::new();
    for device in devices {
        match device.kind {
            nexo_media::AudioDeviceKind::Input => input.push(device),
            nexo_media::AudioDeviceKind::Output => output.push(device),
        }
    }
    (input, output)
}

fn default_device_id(devices: &[nexo_media::AudioDeviceInfo]) -> Option<String> {
    devices
        .iter()
        .find(|device| device.is_default)
        .or_else(|| devices.first())
        .map(|device| device.id.clone())
}

fn display_rows(devices: &[nexo_media::AudioDeviceInfo]) -> Vec<String> {
    let mut used = std::collections::HashMap::<String, usize>::new();
    devices
        .iter()
        .map(|device| {
            let index = used.entry(device.name.clone()).or_insert(0);
            let text = if *index == 0 {
                device.name.clone()
            } else {
                format!("{} ({index})", device.name)
            };
            *index += 1;
            text
        })
        .collect()
}

fn id_for_display(devices: &[nexo_media::AudioDeviceInfo], display: &str) -> Option<String> {
    display_rows(devices)
        .iter()
        .position(|row| row == display)
        .map(|index| devices[index].id.clone())
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
fn start_discovery(
    app: slint::Weak<AppWindow>,
    identity: DeviceIdentity,
    database_path: PathBuf,
    listen_addresses: Arc<Mutex<Vec<String>>>,
    network_peer_id: Arc<Mutex<Option<String>>>,
    active_relay_peer_id: Arc<Mutex<Option<String>>>,
    mut dial_requests: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut call_requests: tokio::sync::mpsc::UnboundedReceiver<CallCommand>,
    mut direct_requests: tokio::sync::mpsc::UnboundedReceiver<DirectCommand>,
    mut moderation_requests: tokio::sync::mpsc::UnboundedReceiver<ModerationCommand>,
    mut file_requests: tokio::sync::mpsc::UnboundedReceiver<FileCommand>,
    mut voice_requests: tokio::sync::mpsc::UnboundedReceiver<VoiceCommand>,
    participants: Arc<Mutex<Vec<nexo_media::ParticipantStatus>>>,
    video_ui: VideoUiDispatcher,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                update_status(&app, format!("Falha ao iniciar rede: {error}"));
                return;
            }
        };
        let status_app = app.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(async move {
            let store = match LocalStore::open(&database_path) {
                Ok(store) => store,
                Err(error) => {
                    update_status(&app, format!("Sincronizacao local indisponivel: {error}"));
                    return;
                }
            };
            let download_dir = database_path
                .parent()
                .map_or_else(|| PathBuf::from("downloads"), |path| path.join("downloads"));
            let mut discovery = match DiscoveryService::start(&identity) {
                Ok(discovery) => discovery,
                Err(error) => {
                    update_status(&app, format!("Descoberta local indisponivel: {error}"));
                    return;
                }
            };
            let local_peer_id = discovery.local_peer_id().to_string();
            if let Ok(mut current_peer_id) = network_peer_id.lock() {
                *current_peer_id = Some(local_peer_id.clone());
            }
            let hardware_encoder = CallEngine::hardware_video_encoder_available();
            let mut metrics_sampler = LocalMetricsSampler::new(&local_peer_id, hardware_encoder);
            let mut local_metrics = metrics_sampler.snapshot();
            if let Err(error) = publish_sync_tokens(&discovery, &store).await {
                update_status(&app, format!("Falha ao preparar sincronizacao: {error}"));
            }
            update_status(&app, "Preparando rede local".to_owned());
            let mut sync_interval = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut media_interval = tokio::time::interval(std::time::Duration::from_millis(10));
            let mut connected_peers = HashSet::new();
            let mut sync_reoffers = HashSet::new();
            let mut incoming_files = HashMap::<uuid::Uuid, IncomingFile>::new();
            let voice_dir = database_path
                .parent()
                .map_or_else(|| PathBuf::from("voice-notes"), |path| path.join("voice-notes"));
            let mut voice_recorder = None::<(uuid::Uuid, uuid::Uuid, VoiceRecorder)>;
            let mut active_call: Option<(uuid::Uuid, uuid::Uuid)> = None;
            let mut call_engine: Option<CallEngine> = None;
            let mut signal_sequence = 0_u64;
            let mut direct_sessions = HashMap::<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>::new();
            let mut sfu_metrics = HashMap::from([(local_peer_id.clone(), local_metrics.clone())]);
            let mut sfu_topology = SfuTopology::new_convergent(ElectionPolicy::default());
            let mut last_sfu_heartbeat = 0_u64;
            let mut topology_call_id = None;
            let mut topology_members = Vec::<String>::new();
            loop {
                let event = tokio::select! {
                    _ = &mut shutdown => break,
                    event = discovery.next_event() => {
                        let Some(event) = event else { break };
                        event
                    }
                    _ = sync_interval.tick() => {
                        if let Ok(tokens) = store.sync_tokens() {
                            let public_tokens = tokens
                                .iter()
                                .map(|(_, token)| *token)
                                .collect::<Vec<_>>();
                            if let Ok(epoch) = store.database_epoch() {
                                let _ = discovery
                                    .update_communities(epoch, public_tokens.clone())
                                    .await;
                            }
                            let _ = discovery
                                .sync_all(SyncRequest::offer(
                                    identity.public_key_bytes(),
                                    public_tokens,
                                ))
                                .await;
                        }
                        // Replay protection is intentionally persistent, but expired call
                        // records no longer need to occupy the local database.
                        let _ = store.prune_old_call_signals(
                            current_timestamp().saturating_sub(10 * 60),
                        );
                        continue;
                    }
                    address = dial_requests.recv() => {
                        if let Some(address) = address {
                            let _ = discovery.dial_invite_address(&address).await;
                        }
                        continue;
                    }
                    command = call_requests.recv() => {
                        if let Some(command) = command {
                            let metrics = local_metrics.clone();
                            Box::pin(handle_call_command(
                                command,
                                &app,
                                &identity,
                                &store,
                                &discovery,
                                &connected_peers,
                                &mut active_call,
                                &mut call_engine,
                                &mut signal_sequence,
                                &metrics,
                            )).await;
                        }
                        continue;
                    }
                    direct = direct_requests.recv() => {
                        if let Some(DirectCommand::Send { community_id, recipient_key, body }) = direct {
                            let status = handle_direct_command(
                                community_id,
                                recipient_key,
                                body,
                                &app,
                                &identity,
                                &store,
                                &discovery,
                                &connected_peers,
                                &mut direct_sessions,
                                &mut signal_sequence,
                            ).await;
                            update_status(&app, status);
                        }
                        continue;
                    }
                    moderation = moderation_requests.recv() => {
                        if let Some(ModerationCommand::RevokeMember { community_id, member_key }) = moderation {
                            let status = revoke_member_from_community(
                                &store,
                                &identity,
                                &discovery,
                                &connected_peers,
                                community_id,
                                member_key,
                            )
                            .await
                            .unwrap_or_else(|error| format!("Falha ao remover membro: {error}"));
                            update_status(&app, status);
                        }
                        continue;
                    }
                    file = file_requests.recv() => {
                        if let Some(FileCommand::Send { community_id, channel_id, path }) = file {
                            let result = send_file_via_available_transport(
                                &store,
                                &discovery,
                                &identity,
                                community_id,
                                channel_id,
                                path,
                                &connected_peers,
                                active_call,
                                call_engine.as_ref(),
                            ).await;
                            update_status(&app, result.unwrap_or_else(|error| {
                                format!("Falha ao enviar arquivo: {error}")
                            }));
                        }
                        continue;
                    }
                    voice = voice_requests.recv() => {
                        match voice {
                            Some(VoiceCommand::Start { community_id, channel_id, input_device }) => {
                                match VoiceRecorder::start(input_device.as_deref()) {
                                    Ok(recorder) => {
                                        voice_recorder = Some((community_id, channel_id, recorder));
                                        update_status(&app, "Gravando nota de voz...".to_owned());
                                    }
                                    Err(error) => {
                                        update_status(
                                            &app,
                                            format!("Falha ao iniciar nota de voz: {error}"),
                                        );
                                        let _ = slint::invoke_from_event_loop({
                                            let app = app.clone();
                                            move || {
                                                if let Some(app) = app.upgrade() {
                                                    app.set_is_recording_voice(false);
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                            Some(VoiceCommand::Stop) => {
                                if let Some((community_id, channel_id, recorder)) = voice_recorder.take() {
                                    match recorder.finish(&voice_dir) {
                                        Ok(path) => {
                                            let result = send_file_via_available_transport(
                                                &store,
                                                &discovery,
                                                &identity,
                                                community_id,
                                                channel_id,
                                                path,
                                                &connected_peers,
                                                active_call,
                                                call_engine.as_ref(),
                                            )
                                            .await;
                                            update_status(
                                                &app,
                                                result.unwrap_or_else(|error| {
                                                    format!("Falha ao enviar nota de voz: {error}")
                                                }),
                                            );
                                        }
                                        Err(error) => update_status(
                                            &app,
                                            format!("Falha ao salvar nota de voz: {error}"),
                                        ),
                                    }
                                }
                                let _ = slint::invoke_from_event_loop({
                                    let app = app.clone();
                                    move || {
                                        if let Some(app) = app.upgrade() {
                                            app.set_is_recording_voice(false);
                                        }
                                    }
                                });
                            }
                            None => {}
                        }
                        continue;
                    }
                    _ = media_interval.tick() => {
                        metrics_sampler.refresh();
                        local_metrics = metrics_sampler.snapshot();
                        sfu_metrics.insert(local_peer_id.clone(), local_metrics.clone());
                        if let Some((_, _, recorder)) = voice_recorder.as_mut()
                            && let Err(error) = recorder.poll()
                        {
                            voice_recorder = None;
                            update_status(&app, format!("Gravacao interrompida: {error}"));
                            let _ = slint::invoke_from_event_loop({
                                let app = app.clone();
                                move || {
                                    if let Some(app) = app.upgrade() {
                                        app.set_is_recording_voice(false);
                                    }
                                }
                            });
                        }
                        if let Some(engine) = call_engine.as_mut() {
                            if let Some((community_id, call_id)) = active_call {
                                if topology_call_id != Some(call_id) {
                                    sfu_metrics.clear();
                                    sfu_metrics.insert(local_peer_id.clone(), local_metrics.clone());
                                    sfu_topology = SfuTopology::new_convergent(ElectionPolicy::default());
                                    last_sfu_heartbeat = 0;
                                    topology_call_id = Some(call_id);
                                    topology_members.clear();
                                    if let Ok(mut current_relay) = active_relay_peer_id.lock() {
                                        *current_relay = None;
                                    }
                                }
                                let mut call_members = engine.call_peer_ids(call_id);
                                call_members.push(local_peer_id.clone());
                                call_members.sort_unstable();
                                call_members.dedup();
                                if topology_members != call_members {
                                    topology_members = call_members.clone();
                                    sfu_metrics.clear();
                                    sfu_metrics.insert(local_peer_id.clone(), local_metrics.clone());
                                    sfu_topology = SfuTopology::new_convergent(ElectionPolicy::default());
                                    last_sfu_heartbeat = 0;
                                    if let Ok(mut current_relay) = active_relay_peer_id.lock() {
                                        *current_relay = None;
                                    }
                                }
                                if sfu_topology.active_host().is_none()
                                    && let Some(host_id) = call_members.first()
                                {
                                    let _ = sfu_topology.establish_initial_host(
                                        host_id,
                                        call_members.get(1).map(String::as_str),
                                    );
                                }
                                let now = current_timestamp();
                                sfu_topology.record_heartbeat(&local_peer_id, now);
                                let metrics_ready = call_members
                                    .iter()
                                    .all(|peer_id| sfu_metrics.contains_key(peer_id));
                                if metrics_ready {
                                    // Only metrics for peers in this call may influence its
                                    // election. The signal map can also contain an authorized
                                    // peer that is connected but has not joined this call.
                                    let nodes = metrics_for_call(&call_members, &sfu_metrics);
                                    let allow_capacity_migration = sfu_topology
                                        .active_host()
                                        .is_none_or(|host| host == local_peer_id.as_str());
                                    let mut topology_events = sfu_topology.update_with_role(
                                        &nodes,
                                        now,
                                        allow_capacity_migration,
                                    );
                                    topology_events.extend(sfu_topology.check_heartbeat_timeout(now));
                                    if let SfuMigrationState::Migrating { target_host, .. } =
                                        sfu_topology.migration_state()
                                        && (target_host == &local_peer_id
                                            || engine.is_peer_connected(target_host, call_id))
                                        && let Some(event) = sfu_topology.confirm_migration()
                                    {
                                        topology_events.push(event);
                                    }
                                    for event in &topology_events {
                                        if let SfuTopologyEvent::MigrationStarted { from, to } = event
                                            && from == &local_peer_id
                                            && let Some(proposal) = SfuMigrationProposal::new(
                                                sfu_topology.term(),
                                                from.clone(),
                                                to.clone(),
                                            )
                                        {
                                            for peer_id in connected_peers.iter().filter(|peer_id| {
                                                is_authorized_peer(&store, community_id, **peer_id)
                                            }) {
                                                let _ = send_call_signal(
                                                    &discovery,
                                                    &identity,
                                                    *peer_id,
                                                    community_id,
                                                    call_id,
                                                    &mut signal_sequence,
                                                    CallSignalKind::SfuMigration,
                                                    proposal.to_signal_payload(),
                                                )
                                                .await;
                                            }
                                        }
                                    }
                                    let route_host = match sfu_topology.migration_state() {
                                        SfuMigrationState::Stable => sfu_topology.active_host(),
                                        SfuMigrationState::Migrating { target_host, .. } => {
                                            Some(target_host.as_str())
                                        }
                                    };
                                    if let Ok(mut current_relay) = active_relay_peer_id.lock() {
                                        *current_relay = route_host.map(str::to_owned);
                                    }
                                    configure_call_topology(
                                        engine,
                                        &local_peer_id,
                                        call_id,
                                        route_host,
                                    );
                                    // A topology event is secondary UI feedback. Do not overwrite the
                                    // media state while the local call is still waiting for its first
                                    // WebRTC participant; that ordering differs across audio backends.
                                    if engine.connected_peer_count() > 0 {
                                        for event in topology_events {
                                            let text = match event {
                                                SfuTopologyEvent::HostElected { host_id } => {
                                                    format!("Relay eleito: {}", short_id(&host_id))
                                                }
                                                SfuTopologyEvent::StandbyElected { standby_id } => {
                                                    format!("Relay reserva: {}", short_id(&standby_id))
                                                }
                                                SfuTopologyEvent::MigrationStarted { to, .. } => {
                                                    format!("Migrando relay para {}", short_id(&to))
                                                }
                                                SfuTopologyEvent::MigrationCompleted { new_host } => {
                                                    format!("Relay migrado para {}", short_id(&new_host))
                                                }
                                                SfuTopologyEvent::HostTimedOut { failed_host } => {
                                                    format!("Relay indisponivel: {}", short_id(&failed_host))
                                                }
                                            };
                                            update_call_status(&app, text);
                                        }
                                    }
                                }
                                if now.saturating_sub(last_sfu_heartbeat) >= 2 {
                                    for peer_id in connected_peers.iter().filter(|peer_id| {
                                        is_authorized_peer(&store, community_id, **peer_id)
                                    }) {
                                        let _ = send_call_signal(
                                            &discovery,
                                            &identity,
                                            *peer_id,
                                            community_id,
                                            call_id,
                                            &mut signal_sequence,
                                            CallSignalKind::SfuMetrics,
                                            local_metrics.signal_payload(),
                                        )
                                        .await;
                                        let _ = send_call_signal(
                                            &discovery,
                                            &identity,
                                            *peer_id,
                                            community_id,
                                            call_id,
                                            &mut signal_sequence,
                                            CallSignalKind::SfuHeartbeat,
                                            "heartbeat".to_owned(),
                                        )
                                        .await;
                                    }
                                    last_sfu_heartbeat = now;
                                }
                            }
                            let states = engine.participant_status();
                            if let Ok(mut shared) = participants.lock() {
                                *shared = states;
                            }
                            let tick_result = std::panic::AssertUnwindSafe(engine.tick())
                                .catch_unwind()
                                .await;
                            let mut media_panicked = false;
                            match tick_result {
                                Ok(Ok(events)) => {
                                    for event in &events {
                                        match event {
                                            CallEngineEvent::LocalVideoFrame { width, height, rgba } => {
                                                queue_local_video(
                                                    &app,
                                                    &video_ui,
                                                    *width,
                                                    *height,
                                                    rgba.clone(),
                                                );
                                            }
                                            CallEngineEvent::RemoteVideoFrame { peer_id, width, height, rgba } => {
                                                queue_remote_video(
                                                    &app,
                                                    &video_ui,
                                                    peer_id,
                                                    *width,
                                                    *height,
                                                    rgba.clone(),
                                                );
                                            }
                                            CallEngineEvent::DataMessage { peer_id, data } => {
                                                let file_status = handle_incoming_webrtc_file_message(
                                                    &store,
                                                    &mut incoming_files,
                                                    &download_dir,
                                                    peer_id,
                                                    data,
                                                )
                                                .unwrap_or_else(|error| {
                                                    format!("Transferencia WebRTC recusada: {error}")
                                                });
                                                update_status(&app, file_status);
                                            }
                                            CallEngineEvent::VideoUnavailable { .. } => {
                                                let app_weak = app.clone();
                                                let _ = slint::invoke_from_event_loop(move || {
                                                    if let Some(app) = app_weak.upgrade() {
                                                        app.set_has_local_video(false);
                                                        app.set_screen_sharing(false);
                                                    }
                                                });
                                            }
                                            _ => {}
                                        }
                                    }
                                    if !events.is_empty() {
                                        update_call_status(&app, call_engine_status(&events, engine));
                                    }
                                }
                                Ok(Err(error)) => {
                                    update_call_status(&app, format!("Falha no audio/video: {error}"));
                                }
                                Err(_) => {
                                    media_panicked = true;
                                    update_call_status(
                                        &app,
                                        "A chamada foi encerrada por uma falha do dispositivo de mídia"
                                            .to_owned(),
                                    );
                                }
                            }
                            if media_panicked {
                                let _ = std::panic::AssertUnwindSafe(engine.close())
                                    .catch_unwind()
                                    .await;
                                call_engine = None;
                                active_call = None;
                                if let Ok(mut shared) = participants.lock() {
                                    shared.clear();
                                }
                                let app_weak = app.clone();
                                let _ = slint::invoke_from_event_loop(move || {
                                    if let Some(app) = app_weak.upgrade() {
                                        app.set_call_starting(false);
                                        app.set_call_active(false);
                                        app.set_call_muted(false);
                                        app.set_video_enabled(false);
                                        app.set_screen_sharing(false);
                                        app.set_has_local_video(false);
                                        app.set_has_remote_video(false);
                                        app.set_remote_videos(
                                            ModelRc::new(VecModel::<RemoteVideoRow>::default()),
                                        );
                                        app.set_call_status("Fora da voz".into());
                                    }
                                });
                            }
                        } else if let Ok(mut shared) = participants.lock() {
                            shared.clear();
                            topology_call_id = None;
                            sfu_metrics.clear();
                            sfu_metrics.insert(local_peer_id.clone(), local_metrics.clone());
                            sfu_topology = SfuTopology::new_convergent(ElectionPolicy::default());
                            if let Ok(mut current_relay) = active_relay_peer_id.lock() {
                                *current_relay = None;
                            }
                        }
                        continue;
                    }
                };
                let status = match event {
                    DiscoveryEvent::Listening(address) => {
                        let relay_address = address.iter().any(|protocol| {
                            matches!(protocol, libp2p::multiaddr::Protocol::P2pCircuit)
                        });
                        remember_listen_address(
                            &listen_addresses,
                            &address,
                            discovery.local_peer_id(),
                        );
                        if relay_address {
                            "Relay NAT ativo".to_owned()
                        } else {
                            "Rede local ativa".to_owned()
                        }
                    }
                    DiscoveryEvent::NetworkWarning(message) => message,
                    DiscoveryEvent::PeerFound { peer_id, .. } => {
                        format!("Pessoa proxima: {}", short_id(&peer_id.to_string()))
                    }
                    DiscoveryEvent::PeerConnected(peer_id) => {
                        connected_peers.insert(peer_id);
                        let _ = publish_sync_tokens(&discovery, &store).await;
                        if let Ok(tokens) = store.sync_tokens() {
                            let offer = SyncRequest::offer(
                                identity.public_key_bytes(),
                                tokens.into_iter().map(|(_, token)| token).collect(),
                            );
                            let _ = discovery.sync_peer(peer_id, offer).await;
                        }
                        let _ = bootstrap_direct_sessions(
                            peer_id,
                            &identity,
                            &store,
                            &discovery,
                            &mut direct_sessions,
                            &mut signal_sequence,
                        )
                        .await;
                        if let Some((community_id, call_id)) = active_call
                            && is_authorized_peer(&store, community_id, peer_id)
                        {
                            let capabilities = call_engine.as_ref().map_or_else(
                                || "video=vp8".to_owned(),
                                CallEngine::local_capabilities_payload,
                            );
                            let _ = send_call_signal(
                                &discovery,
                                &identity,
                                peer_id,
                                community_id,
                                call_id,
                                &mut signal_sequence,
                                CallSignalKind::Capabilities,
                                capabilities,
                            )
                            .await;
                            let _ = send_call_signal(
                                &discovery,
                                &identity,
                                peer_id,
                                community_id,
                                call_id,
                                &mut signal_sequence,
                                CallSignalKind::SfuMetrics,
                                local_metrics.signal_payload(),
                            )
                            .await;
                            let _ = send_call_signal(
                                &discovery,
                                &identity,
                                peer_id,
                                community_id,
                                call_id,
                                &mut signal_sequence,
                                CallSignalKind::ParticipantState,
                                "join".to_owned(),
                            ).await;
                        }
                        format!("Conexao direta: {}", short_id(&peer_id.to_string()))
                    }
                    DiscoveryEvent::SyncWanted {
                        peer_id,
                        device_key,
                        receiver_epoch,
                        tokens,
                    } => {
                        match build_sync_batch(
                            &store,
                            &identity,
                            &peer_id.to_string(),
                            Some(device_key),
                            receiver_epoch,
                            &tokens,
                        ) {
                            Ok(batch) => {
                                let _ = discovery.sync_peer(peer_id, batch).await;
                                "Sincronizando historico".to_owned()
                            }
                            Err(error) => format!("Falha ao preparar historico: {error}"),
                        }
                    }
                    DiscoveryEvent::SyncReceived { peer_id, request } => {
                        match apply_sync_batch(
                            &store,
                            &identity,
                            &mut direct_sessions,
                            &request,
                            ) {
                                Ok((
                                    inserted,
                                    receiver_epoch,
                                    acknowledgements,
                                    changed_communities,
                                )) => {
                                if let Err(error) = refresh_call_membership_after_sync(
                                    &app,
                                    &identity,
                                    &store,
                                    &connected_peers,
                                    &mut active_call,
                                    &mut call_engine,
                                    &changed_communities,
                                )
                                .await
                                {
                                    update_call_status(
                                        &app,
                                        format!("Falha ao atualizar seguranca da chamada: {error}"),
                                    );
                                }
                                let ack = SyncRequest::ack(
                                    identity.public_key_bytes(),
                                    receiver_epoch,
                                    acknowledgements,
                                );
                                let _ = discovery.sync_peer(peer_id, ack).await;
                                if sync_reoffers.insert(peer_id)
                                    && let Ok(tokens) = store.sync_tokens()
                                {
                                    let offer = SyncRequest::offer(
                                        identity.public_key_bytes(),
                                        tokens.into_iter().map(|(_, token)| token).collect(),
                                    );
                                    let _ = discovery.sync_peer(peer_id, offer).await;
                                }
                                let _ = bootstrap_direct_sessions(
                                    peer_id,
                                    &identity,
                                    &store,
                                    &discovery,
                                    &mut direct_sessions,
                                    &mut signal_sequence,
                                )
                                .await;
                                if let Some((community_id, call_id)) = active_call
                                    && is_authorized_peer(&store, community_id, peer_id)
                                {
                                    let capabilities = call_engine.as_ref().map_or_else(
                                        || "video=vp8".to_owned(),
                                        CallEngine::local_capabilities_payload,
                                    );
                                    let _ = send_call_signal(
                                        &discovery,
                                        &identity,
                                        peer_id,
                                        community_id,
                                        call_id,
                                        &mut signal_sequence,
                                        CallSignalKind::Capabilities,
                                        capabilities,
                                    )
                                    .await;
                                    let _ = send_call_signal(
                                        &discovery,
                                        &identity,
                                        peer_id,
                                        community_id,
                                        call_id,
                                        &mut signal_sequence,
                                        CallSignalKind::SfuMetrics,
                                        local_metrics.signal_payload(),
                                    )
                                    .await;
                                    let _ = send_call_signal(
                                        &discovery,
                                        &identity,
                                        peer_id,
                                        community_id,
                                        call_id,
                                        &mut signal_sequence,
                                        CallSignalKind::ParticipantState,
                                        "join".to_owned(),
                                    ).await;
                                }
                                format!("Sincronizacao concluida: {inserted} novas")
                            }
                            Err(error) => {
                                if sync_reoffers.insert(peer_id)
                                    && let Ok(tokens) = store.sync_tokens()
                                {
                                    let offer = SyncRequest::offer(
                                        identity.public_key_bytes(),
                                        tokens.into_iter().map(|(_, token)| token).collect(),
                                    );
                                    let _ = discovery.sync_peer(peer_id, offer).await;
                                }
                                format!("Lote de sincronizacao recusado: {error}")
                            }
                        }
                    }
                    DiscoveryEvent::SyncAcknowledged { peer_id, request } => {
                        match handle_sync_ack(&store, &identity, &peer_id.to_string(), &request) {
                            Ok(Some(batch)) => {
                                let _ = discovery.sync_peer(peer_id, batch).await;
                                "Sincronizando proxima pagina".to_owned()
                            }
                            Ok(None) => "Historico sincronizado".to_owned(),
                            Err(error) => format!("Falha ao confirmar sincronizacao: {error}"),
                        }
                    }
                    DiscoveryEvent::CallSignalsReceived { peer_id, request } => {
                        Box::pin(handle_call_signals(
                            peer_id,
                            request,
                            &app,
                            &identity,
                            &store,
                            &discovery,
                            active_call,
                            &mut call_engine,
                            &mut signal_sequence,
                            &mut sfu_metrics,
                            &mut sfu_topology,
                            &mut direct_sessions,
                        )).await
                    }
                    DiscoveryEvent::FileOfferReceived {
                        peer_id,
                        offer,
                        channel,
                    } => handle_incoming_file_offer(
                        &store,
                        &discovery,
                        &mut incoming_files,
                        &download_dir,
                        peer_id,
                        offer,
                        channel,
                    )
                    .await
                    .unwrap_or_else(|error| format!("Arquivo recusado: {error}")),
                    DiscoveryEvent::FileResponseReceived { peer_id, response } => {
                        handle_file_response(
                            &store,
                            &discovery,
                            &mut incoming_files,
                            peer_id,
                            response,
                        )
                        .await
                        .unwrap_or_else(|error| format!("Transferência de arquivo: {error}"))
                    }
                    DiscoveryEvent::PeerExpired { peer_id, .. } => {
                        let peer_name = peer_id.to_string();
                        sfu_metrics.remove(&peer_name);
                        sfu_topology.remove_node(&peer_name);
                        connected_peers.remove(&peer_id);
                        sync_reoffers.remove(&peer_id);
                        if let Some(engine) = call_engine.as_mut() {
                            let _ = Box::pin(engine.remove_peer(&peer_name)).await;
                        }
                        "Procurando na rede local".to_owned()
                    }
                    DiscoveryEvent::PeerDisconnected(peer_id) => {
                        connected_peers.remove(&peer_id);
                        sync_reoffers.remove(&peer_id);
                        let peer_name = peer_id.to_string();
                        sfu_metrics.remove(&peer_name);
                        sfu_topology.remove_node(&peer_name);
                        if let Some(engine) = call_engine.as_mut() {
                            let _ = Box::pin(engine.remove_peer(&peer_name)).await;
                        }
                        "Procurando na rede local".to_owned()
                    }
                };
                update_status(&app, status);
            }
            });
        }));
        if result.is_err() {
            update_status(
                &status_app,
                "O serviço de rede/mídia encontrou um erro interno e foi reiniciado".to_owned(),
            );
        }
    })
}

async fn send_file_from_path(
    store: &LocalStore,
    discovery: &DiscoveryService,
    identity: &DeviceIdentity,
    community_id: uuid::Uuid,
    channel_id: uuid::Uuid,
    path: PathBuf,
    connected_peers: &HashSet<libp2p::PeerId>,
) -> Result<String> {
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("nao foi possivel ler {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("o caminho escolhido nao e um arquivo");
    }
    if metadata.len() > MAX_FILE_BYTES {
        anyhow::bail!("o arquivo excede o limite de 256 MB");
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("nao foi possivel abrir {}", path.display()))?;
    let community = store
        .communities()?
        .into_iter()
        .find(|community| community.id == community_id)
        .context("comunidade selecionada nao existe")?;
    if !store
        .channels(community_id)?
        .iter()
        .any(|channel| channel.id == channel_id)
    {
        anyhow::bail!("canal selecionado nao existe nesta comunidade");
    }
    let peers = connected_peers
        .iter()
        .copied()
        .filter(|peer_id| is_authorized_peer(store, community_id, *peer_id))
        .collect::<Vec<_>>();
    if peers.is_empty() {
        anyhow::bail!("nenhum participante autorizado esta conectado");
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "arquivo.bin".to_owned(), str::to_owned);
    let mime_type = mime_type_for_name(&file_name);
    let now = current_timestamp();
    let offer = FileTransferOffer::create(
        identity,
        community.id,
        channel_id,
        file_name.clone(),
        data.len() as u64,
        mime_type,
        compute_sha256(&data),
        now,
    )?;
    let mut chunks = Vec::new();
    if data.is_empty() {
        chunks.push(FileChunk::new(offer.id, 0, Vec::new()));
    } else {
        for (index, part) in data.chunks(DEFAULT_FILE_CHUNK_SIZE as usize).enumerate() {
            chunks.push(FileChunk::new(
                offer.id,
                u32::try_from(index).context("quantidade de chunks excede o limite")?,
                part.to_vec(),
            ));
        }
    }
    store.record_file_offer(
        &offer,
        Some(path.to_string_lossy().as_ref()),
        "available",
        now,
    )?;
    discovery.broadcast_file(offer, chunks, peers).await?;
    Ok(format!("Arquivo '{file_name}' oferecido aos participantes"))
}

/// Prefer the active WebRTC data channels for attachments and voice notes.
/// The libp2p request/response stream remains available when some authorized
/// community members are online but have not joined the call.
#[allow(clippy::too_many_arguments)]
async fn send_file_via_available_transport(
    store: &LocalStore,
    discovery: &DiscoveryService,
    identity: &DeviceIdentity,
    community_id: uuid::Uuid,
    channel_id: uuid::Uuid,
    path: PathBuf,
    connected_peers: &HashSet<libp2p::PeerId>,
    active_call: Option<(uuid::Uuid, uuid::Uuid)>,
    call_engine: Option<&CallEngine>,
) -> Result<String> {
    if let Some(engine) = call_engine
        && active_call.is_some_and(|(active_community, _)| active_community == community_id)
        && let Some(result) = send_file_over_webrtc(
            store,
            identity,
            community_id,
            channel_id,
            path.clone(),
            connected_peers,
            active_call.map(|(_, call_id)| call_id),
            engine,
        )
        .await?
    {
        return Ok(result);
    }
    send_file_from_path(
        store,
        discovery,
        identity,
        community_id,
        channel_id,
        path,
        connected_peers,
    )
    .await
}

/// Send one attachment to every currently connected authorized call peer.
/// `None` means that the call cannot cover the whole audience and the caller
/// should use the libp2p fallback. Once the first WebRTC peer accepts the
/// offer, later failures are returned as errors instead of duplicating a
/// partially sent transfer through the fallback.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn send_file_over_webrtc(
    store: &LocalStore,
    identity: &DeviceIdentity,
    community_id: uuid::Uuid,
    channel_id: uuid::Uuid,
    path: PathBuf,
    connected_peers: &HashSet<libp2p::PeerId>,
    call_id: Option<uuid::Uuid>,
    engine: &CallEngine,
) -> Result<Option<String>> {
    let Some(call_id) = call_id else {
        return Ok(None);
    };
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("nao foi possivel ler {}", path.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("o caminho escolhido nao e um arquivo");
    }
    if metadata.len() > MAX_FILE_BYTES {
        anyhow::bail!("o arquivo excede o limite de 256 MB");
    }
    let data = std::fs::read(&path)
        .with_context(|| format!("nao foi possivel abrir {}", path.display()))?;
    let community = store
        .communities()?
        .into_iter()
        .find(|community| community.id == community_id)
        .context("comunidade selecionada nao existe")?;
    if !store
        .channels(community_id)?
        .iter()
        .any(|channel| channel.id == channel_id)
    {
        anyhow::bail!("canal selecionado nao existe nesta comunidade");
    }

    let authorized_online = connected_peers
        .iter()
        .filter(|peer_id| is_authorized_peer(store, community_id, **peer_id))
        .map(ToString::to_string)
        .collect::<HashSet<_>>();
    let call_peers = engine
        .call_peer_ids(call_id)
        .into_iter()
        .filter(|peer_id| authorized_online.contains(peer_id))
        .collect::<HashSet<_>>();
    if call_peers.is_empty() || call_peers.len() != authorized_online.len() {
        return Ok(None);
    }

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "arquivo.bin".to_owned(), str::to_owned);
    let mime_type = mime_type_for_name(&file_name);
    let now = current_timestamp();
    let offer = FileTransferOffer::create_with_chunk_size(
        identity,
        community.id,
        channel_id,
        file_name.clone(),
        data.len() as u64,
        mime_type,
        compute_sha256(&data),
        now,
        WEBRTC_FILE_CHUNK_SIZE,
    )?;
    let mut chunks = Vec::new();
    if data.is_empty() {
        chunks.push(FileChunk::new(offer.id, 0, Vec::new()));
    } else {
        for (index, part) in data.chunks(WEBRTC_FILE_CHUNK_SIZE as usize).enumerate() {
            chunks.push(FileChunk::new(
                offer.id,
                u32::try_from(index).context("quantidade de chunks excede o limite")?,
                part.to_vec(),
            ));
        }
    }
    let offer_message = serde_json::to_vec(&WebRtcFileMessage::Offer(offer.clone()))?;
    anyhow::ensure!(
        offer_message.len() <= DATA_CHANNEL_MESSAGE_BYTES,
        "metadados do arquivo excedem o limite do canal WebRTC"
    );
    let chunk_messages = chunks
        .iter()
        .map(|chunk| {
            serde_json::to_vec(&WebRtcFileMessage::Chunk {
                transfer_id: chunk.transfer_id,
                chunk_index: chunk.chunk_index,
                data_base64: BASE64.encode(&chunk.data),
                chunk_sha256: chunk.chunk_sha256,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    anyhow::ensure!(
        chunk_messages
            .iter()
            .all(|message| message.len() <= DATA_CHANNEL_MESSAGE_BYTES),
        "chunk do arquivo excede o limite do canal WebRTC"
    );

    let mut sent_peers = 0_usize;
    for peer_id in call_peers {
        if let Err(error) = engine.send_data_to(&peer_id, &offer_message).await {
            if sent_peers == 0 {
                return Ok(None);
            }
            return Err(error.into());
        }
        if sent_peers == 0 {
            store.record_file_offer(
                &offer,
                Some(path.to_string_lossy().as_ref()),
                "available",
                now,
            )?;
        }
        for message in &chunk_messages {
            engine.send_data_to(&peer_id, message).await?;
        }
        sent_peers = sent_peers.saturating_add(1);
    }
    Ok(Some(format!(
        "Arquivo '{file_name}' enviado via WebRTC para {sent_peers} participante(s)"
    )))
}

#[allow(clippy::too_many_lines)]
fn handle_incoming_webrtc_file_message(
    store: &LocalStore,
    incoming_files: &mut HashMap<uuid::Uuid, IncomingFile>,
    download_dir: &Path,
    peer_id: &str,
    data: &[u8],
) -> Result<String> {
    let peer_id = peer_id
        .parse::<libp2p::PeerId>()
        .context("peer WebRTC invalido")?;
    let message: WebRtcFileMessage =
        serde_json::from_slice(data).context("mensagem de arquivo WebRTC invalida")?;
    match message {
        WebRtcFileMessage::Offer(offer) => {
            offer.verify(current_timestamp())?;
            let author_peer = peer_id_for_device_key(&offer.author_key)
                .context("chave do autor nao corresponde a um peer")?;
            anyhow::ensure!(
                author_peer == peer_id,
                "autor do arquivo nao corresponde ao peer"
            );
            let expected_chunks = if offer.file_size == 0 {
                1
            } else {
                u32::try_from(offer.file_size.div_ceil(u64::from(offer.chunk_size))).unwrap_or(0)
            };
            anyhow::ensure!(
                is_authorized_peer(store, offer.community_id, peer_id),
                "remetente nao autorizado nesta comunidade"
            );
            anyhow::ensure!(
                offer.chunk_size == WEBRTC_FILE_CHUNK_SIZE
                    && offer.file_size <= MAX_FILE_BYTES
                    && offer.total_chunks > 0
                    && offer.total_chunks == expected_chunks,
                "metadados do arquivo WebRTC fora dos limites"
            );
            anyhow::ensure!(
                store
                    .channels(offer.community_id)?
                    .iter()
                    .any(|channel| channel.id == offer.channel_id),
                "canal do arquivo nao existe"
            );
            anyhow::ensure!(
                !incoming_files.contains_key(&offer.id),
                "transferencia WebRTC duplicada"
            );
            std::fs::create_dir_all(download_dir)?;
            let path = unique_download_path(download_dir, &offer.file_name);
            store.record_file_offer(
                &offer,
                Some(path.to_string_lossy().as_ref()),
                "downloading",
                current_timestamp(),
            )?;
            incoming_files.insert(
                offer.id,
                IncomingFile {
                    peer_id,
                    path,
                    total_chunks: offer.total_chunks,
                    next_chunk: 0,
                },
            );
            Ok(format!(
                "Recebendo arquivo '{}' via WebRTC",
                offer.file_name
            ))
        }
        WebRtcFileMessage::Chunk {
            transfer_id,
            chunk_index,
            data_base64,
            chunk_sha256,
        } => {
            let chunk = FileChunk {
                transfer_id,
                chunk_index,
                data: BASE64
                    .decode(data_base64)
                    .context("payload Base64 do chunk WebRTC invalido")?,
                chunk_sha256,
            };
            let transfer = store
                .file_transfer(chunk.transfer_id)?
                .context("transferencia WebRTC desconhecida")?;
            anyhow::ensure!(
                transfer.chunk_size == WEBRTC_FILE_CHUNK_SIZE,
                "chunk de transferencia WebRTC usa tamanho inesperado"
            );
            let Some(incoming) = incoming_files.get_mut(&chunk.transfer_id) else {
                anyhow::bail!("transferencia WebRTC nao esta ativa");
            };
            anyhow::ensure!(
                incoming.peer_id == peer_id,
                "peer do chunk nao corresponde ao autor"
            );
            anyhow::ensure!(
                chunk.chunk_index == incoming.next_chunk,
                "chunk WebRTC recebido fora de ordem"
            );
            chunk.verify(transfer.total_chunks)?;
            anyhow::ensure!(
                chunk.data.len() <= transfer.chunk_size as usize,
                "chunk WebRTC excede o tamanho anunciado"
            );
            let offset = u64::from(chunk.chunk_index) * u64::from(transfer.chunk_size);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&incoming.path)?;
            file.set_len(transfer.file_size)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&chunk.data)?;
            file.sync_data()?;
            store.record_chunk_received(chunk.transfer_id, chunk.chunk_index)?;
            incoming.next_chunk = incoming.next_chunk.saturating_add(1);
            if incoming.next_chunk < incoming.total_chunks {
                return Ok(format!(
                    "Recebendo arquivo via WebRTC: {}/{} chunks",
                    incoming.next_chunk, incoming.total_chunks
                ));
            }
            let bytes = std::fs::read(&incoming.path)?;
            if bytes.len() as u64 != transfer.file_size
                || compute_sha256(&bytes) != transfer.root_sha256
            {
                store.update_file_transfer_status(chunk.transfer_id, "failed", None)?;
                incoming_files.remove(&chunk.transfer_id);
                anyhow::bail!("hash final do arquivo WebRTC nao confere");
            }
            let path = incoming.path.clone();
            store.update_file_transfer_status(
                chunk.transfer_id,
                "completed",
                Some(path.to_string_lossy().as_ref()),
            )?;
            incoming_files.remove(&chunk.transfer_id);
            Ok(format!("Arquivo recebido via WebRTC em {}", path.display()))
        }
    }
}

async fn handle_incoming_file_offer(
    store: &LocalStore,
    discovery: &DiscoveryService,
    incoming_files: &mut HashMap<uuid::Uuid, IncomingFile>,
    download_dir: &Path,
    peer_id: libp2p::PeerId,
    offer: FileTransferOffer,
    channel: FileOfferResponseChannel,
) -> Result<String> {
    let mut rejection = None;
    if !is_authorized_peer(store, offer.community_id, peer_id) {
        rejection = Some("remetente nao autorizado nesta comunidade");
    }
    let expected_chunks = if offer.file_size == 0 {
        1
    } else {
        offer.file_size.div_ceil(u64::from(offer.chunk_size.max(1)))
    };
    if rejection.is_none()
        && (offer.file_size > MAX_FILE_BYTES
            || offer.chunk_size != DEFAULT_FILE_CHUNK_SIZE
            || expected_chunks != u64::from(offer.total_chunks)
            || offer.total_chunks == 0)
    {
        rejection = Some("metadados do arquivo fora dos limites");
    }
    if let Some(reason) = rejection {
        discovery
            .respond_file_offer(
                channel,
                FileTransferResponse::OfferRejected {
                    reason: reason.to_owned(),
                },
            )
            .await?;
        anyhow::bail!(reason);
    }
    std::fs::create_dir_all(download_dir)?;
    let path = unique_download_path(download_dir, &offer.file_name);
    store.record_file_offer(
        &offer,
        Some(path.to_string_lossy().as_ref()),
        "downloading",
        current_timestamp(),
    )?;
    discovery
        .respond_file_offer(
            channel,
            FileTransferResponse::OfferAccepted {
                transfer_id: offer.id,
            },
        )
        .await?;
    incoming_files.insert(
        offer.id,
        IncomingFile {
            peer_id,
            path,
            total_chunks: offer.total_chunks,
            next_chunk: 0,
        },
    );
    discovery.request_file_chunk(peer_id, offer.id, 0).await?;
    Ok(format!("Recebendo arquivo '{}'", offer.file_name))
}

async fn handle_file_response(
    store: &LocalStore,
    discovery: &DiscoveryService,
    incoming_files: &mut HashMap<uuid::Uuid, IncomingFile>,
    peer_id: libp2p::PeerId,
    response: FileTransferResponse,
) -> Result<String> {
    match response {
        FileTransferResponse::OfferAccepted { transfer_id } => Ok(format!(
            "Arquivo aceito pelo peer {}",
            short_id(&peer_id.to_string())
        ))
        .map(|text| format!("{text} ({transfer_id})")),
        FileTransferResponse::OfferRejected { reason } => Ok(format!("Arquivo recusado: {reason}")),
        FileTransferResponse::Chunk(chunk) => {
            let transfer = store
                .file_transfer(chunk.transfer_id)?
                .context("transferencia desconhecida")?;
            let Some(incoming) = incoming_files.get_mut(&chunk.transfer_id) else {
                anyhow::bail!("transferencia nao esta ativa");
            };
            if incoming.peer_id != peer_id || chunk.chunk_index != incoming.next_chunk {
                anyhow::bail!("chunk recebido fora de ordem");
            }
            chunk.verify(transfer.total_chunks)?;
            if chunk.data.len() > transfer.chunk_size as usize {
                anyhow::bail!("chunk excede o tamanho anunciado");
            }
            let offset = u64::from(chunk.chunk_index) * u64::from(transfer.chunk_size);
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(&incoming.path)?;
            file.set_len(transfer.file_size)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&chunk.data)?;
            file.sync_data()?;
            store.record_chunk_received(chunk.transfer_id, chunk.chunk_index)?;
            incoming.next_chunk = incoming.next_chunk.saturating_add(1);
            if incoming.next_chunk < incoming.total_chunks {
                let next = incoming.next_chunk;
                discovery
                    .request_file_chunk(peer_id, chunk.transfer_id, next)
                    .await?;
                return Ok(format!(
                    "Recebendo arquivo: {}/{} chunks",
                    next, incoming.total_chunks
                ));
            }
            let bytes = std::fs::read(&incoming.path)?;
            if bytes.len() as u64 != transfer.file_size
                || compute_sha256(&bytes) != transfer.root_sha256
            {
                store.update_file_transfer_status(chunk.transfer_id, "failed", None)?;
                incoming_files.remove(&chunk.transfer_id);
                anyhow::bail!("hash final do arquivo nao confere");
            }
            let path = incoming.path.clone();
            store.update_file_transfer_status(
                chunk.transfer_id,
                "completed",
                Some(path.to_string_lossy().as_ref()),
            )?;
            incoming_files.remove(&chunk.transfer_id);
            Ok(format!("Arquivo recebido em {}", path.display()))
        }
        FileTransferResponse::ChunkNotFound => anyhow::bail!("chunk nao encontrado no remetente"),
    }
}

fn unique_download_path(directory: &Path, name: &str) -> PathBuf {
    let safe_name = sanitize_file_name(name);
    let candidate = directory.join(&safe_name);
    if !candidate.exists() {
        return candidate;
    }
    let path = Path::new(&safe_name);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("arquivo");
    let extension = path.extension().and_then(|value| value.to_str());
    for index in 1..10_000_u32 {
        let name = extension.map_or_else(
            || format!("{stem} ({index})"),
            |extension| format!("{stem} ({index}).{extension}"),
        );
        let candidate = directory.join(name);
        if !candidate.exists() {
            return candidate;
        }
    }
    directory.join(format!("{safe_name}.copy"))
}

fn sanitize_file_name(name: &str) -> String {
    let name = Path::new(name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("arquivo.bin");
    let result = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let result = result.trim().trim_matches('.').to_owned();
    if result.is_empty() {
        "arquivo.bin".to_owned()
    } else {
        result
    }
}

fn mime_type_for_name(name: &str) -> String {
    match Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        Some("pdf") => "application/pdf",
        Some("txt") => "text/plain",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn call_engine_status(events: &[CallEngineEvent], engine: &CallEngine) -> String {
    if events
        .iter()
        .any(|event| matches!(event, CallEngineEvent::AudioInputUnavailable))
    {
        return "Microfone desconectado; tentando reconectar".to_owned();
    }
    if events
        .iter()
        .any(|event| matches!(event, CallEngineEvent::AudioOutputUnavailable))
    {
        return "Saida de audio desconectada; tentando reconectar".to_owned();
    }
    if events
        .iter()
        .any(|event| matches!(event, CallEngineEvent::AudioInputRecovered))
    {
        return "Microfone reconectado".to_owned();
    }
    if events
        .iter()
        .any(|event| matches!(event, CallEngineEvent::AudioOutputRecovered))
    {
        return "Saida de audio reconectada".to_owned();
    }
    if let Some(reason) = events.iter().find_map(|event| match event {
        CallEngineEvent::VideoUnavailable { reason } => Some(reason.as_str()),
        _ => None,
    }) {
        return format!("Video indisponivel: {reason}");
    }
    if events
        .iter()
        .any(|event| matches!(event, CallEngineEvent::VideoRecovered))
    {
        return "Video recuperado".to_owned();
    }
    let connected = engine.connected_peer_count();
    if connected == 0 {
        "Na voz, aguardando pessoas".to_owned()
    } else {
        format!("{connected} pessoa(s) conectada(s)")
    }
}

/// Applies the currently elected participant-hosted SFU route. During a
/// migration the target is used immediately while old WebRTC connections stay
/// alive, which gives the move make-before-break semantics.
fn configure_call_topology(
    engine: &mut CallEngine,
    local_peer_id: &str,
    call_id: uuid::Uuid,
    host_id: Option<&str>,
) {
    let peers = engine.call_peer_ids(call_id);
    let is_host = host_id == Some(local_peer_id);
    let targets = match host_id {
        Some(_host) if is_host => peers
            .into_iter()
            .filter(|peer_id| peer_id != local_peer_id)
            .collect(),
        Some(host) if peers.iter().any(|peer_id| peer_id == host) => vec![host.to_owned()],
        _ => Vec::new(),
    };
    engine.configure_media_topology(is_host, targets);
}

fn metrics_for_call(
    call_members: &[String],
    metrics: &HashMap<String, NodeMetrics>,
) -> Vec<NodeMetrics> {
    call_members
        .iter()
        .filter_map(|peer_id| metrics.get(peer_id).cloned())
        .collect()
}

#[cfg(test)]
fn select_media_targets(local_peer_id: &str, mut peers: Vec<String>) -> (bool, Vec<String>) {
    peers.push(local_peer_id.to_owned());
    peers.sort_unstable();
    peers.dedup();
    let is_host = peers.first().is_some_and(|host| host == local_peer_id);
    let targets = if is_host {
        peers
            .into_iter()
            .filter(|peer_id| peer_id != local_peer_id)
            .collect()
    } else {
        peers.into_iter().next().into_iter().collect()
    };
    (is_host, targets)
}

struct LocalMetricsSampler {
    peer_id: String,
    hardware_encoder: bool,
    gpu_available: bool,
    system: System,
    last_refresh: Instant,
    cpu_headroom_percent: f32,
}

impl LocalMetricsSampler {
    fn new(peer_id: &str, hardware_encoder: bool) -> Self {
        let mut system = System::new();
        system.refresh_cpu_usage();
        let gpu_available = nexo_video::CapabilityProbe::new()
            .probe()
            .gpu_name
            .is_some();
        Self {
            peer_id: peer_id.to_owned(),
            hardware_encoder,
            gpu_available,
            system,
            last_refresh: Instant::now(),
            cpu_headroom_percent: 60.0,
        }
    }

    fn refresh(&mut self) {
        if self.last_refresh.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.system.refresh_cpu_usage();
        let usage = self.system.global_cpu_usage();
        if usage.is_finite() {
            self.cpu_headroom_percent = (100.0 - usage).clamp(0.0, 100.0);
        }
        self.last_refresh = Instant::now();
    }

    fn snapshot(&self) -> NodeMetrics {
        NodeMetrics {
            node_id: self.peer_id.clone(),
            // Upload capacity remains a conservative floor until WebRTC RTCP
            // statistics are aggregated across the active connections.
            available_upload_mbps: if self.hardware_encoder { 100.0 } else { 50.0 },
            packet_loss_percent: 0.0,
            round_trip_ms: 1.0,
            cpu_headroom_percent: self.cpu_headroom_percent,
            gpu_headroom_percent: if self.gpu_available { 70.0 } else { 20.0 },
            hardware_encoder: self.hardware_encoder,
            publicly_reachable: false,
        }
    }
}

fn community_media_secret(store: &LocalStore, community_id: uuid::Uuid) -> Result<[u8; 32]> {
    let group = match store.mls_group(community_id)? {
        Some(group) => group,
        None => ensure_local_mls_state(store, community_id)?,
    };
    Ok(group.derive_application_secret("nexo-media"))
}

/// Re-key an active call after membership sync and remove peers that no
/// longer belong to the community. A revoked local identity must stop
/// publishing immediately, even if its transport connection is still open.
async fn refresh_call_membership_after_sync(
    app: &slint::Weak<AppWindow>,
    identity: &DeviceIdentity,
    store: &LocalStore,
    connected_peers: &HashSet<libp2p::PeerId>,
    active_call: &mut Option<(uuid::Uuid, uuid::Uuid)>,
    call_engine: &mut Option<CallEngine>,
    changed_communities: &[uuid::Uuid],
) -> Result<()> {
    let Some((community_id, call_id)) = *active_call else {
        return Ok(());
    };
    let local_key = identity.public_key_bytes();
    if !store.is_authorized_member(community_id, &local_key)? {
        if let Some(engine) = call_engine.as_mut() {
            let _ = Box::pin(engine.close()).await;
        }
        *call_engine = None;
        *active_call = None;
        let app_weak = app.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(app) = app_weak.upgrade() {
                app.set_call_starting(false);
                app.set_call_active(false);
                app.set_call_muted(false);
                app.set_video_enabled(false);
                app.set_screen_sharing(false);
                app.set_has_local_video(false);
                app.set_has_remote_video(false);
                app.set_remote_videos(ModelRc::new(VecModel::<RemoteVideoRow>::default()));
                app.set_call_status("Acesso à comunidade revogado".into());
            }
        });
        return Ok(());
    }

    if let Some(engine) = call_engine.as_mut() {
        if changed_communities.contains(&community_id) {
            let media_secret = community_media_secret(store, community_id)?;
            engine.set_media_secret(call_id, &media_secret);
        }
        let removed_peers = connected_peers
            .iter()
            .filter(|peer_id| !is_authorized_peer(store, community_id, **peer_id))
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for peer_id in removed_peers {
            let _ = Box::pin(engine.remove_peer(&peer_id)).await;
        }
    }
    Ok(())
}

fn queue_local_video(
    app: &slint::Weak<AppWindow>,
    dispatcher: &VideoUiDispatcher,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) {
    let Some(frame) = valid_video_frame(width, height, rgba) else {
        return;
    };
    if let Ok(mut pending) = dispatcher.pending.lock() {
        pending.local = Some(frame);
    }
    schedule_video_ui(app, dispatcher);
}

fn queue_remote_video(
    app: &slint::Weak<AppWindow>,
    dispatcher: &VideoUiDispatcher,
    peer_id: &str,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
) {
    let Some(frame) = valid_video_frame(width, height, rgba) else {
        return;
    };
    if let Ok(mut pending) = dispatcher.pending.lock() {
        pending.remotes.insert(peer_id.to_owned(), frame);
    }
    schedule_video_ui(app, dispatcher);
}

fn valid_video_frame(width: u32, height: u32, rgba: Vec<u8>) -> Option<PendingVideoFrame> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))?;
    (rgba.len() == expected).then_some(PendingVideoFrame {
        width,
        height,
        rgba,
    })
}

fn schedule_video_ui(app: &slint::Weak<AppWindow>, dispatcher: &VideoUiDispatcher) {
    let delay = {
        let Ok(mut pending) = dispatcher.pending.lock() else {
            return;
        };
        if pending.scheduled {
            return;
        }
        pending.scheduled = true;
        pending
            .last_flush
            .and_then(|last| VIDEO_UI_INTERVAL.checked_sub(last.elapsed()))
            .unwrap_or_default()
    };
    let app = app.clone();
    let dispatcher = dispatcher.clone();
    thread::spawn(move || {
        if !delay.is_zero() {
            thread::sleep(delay);
        }
        let reset_dispatcher = dispatcher.clone();
        if slint::invoke_from_event_loop(move || flush_video_ui(&app, &dispatcher)).is_err()
            && let Ok(mut pending) = reset_dispatcher.pending.lock()
        {
            pending.scheduled = false;
        }
    });
}

fn flush_video_ui(app: &slint::Weak<AppWindow>, dispatcher: &VideoUiDispatcher) {
    let (local, remotes) = {
        let Ok(mut pending) = dispatcher.pending.lock() else {
            return;
        };
        pending.scheduled = false;
        pending.last_flush = Some(Instant::now());
        (pending.local.take(), std::mem::take(&mut pending.remotes))
    };
    let Some(app) = app.upgrade() else {
        return;
    };
    if let Some(frame) = local {
        let mut buffer =
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(frame.width, frame.height);
        buffer.make_mut_bytes().copy_from_slice(&frame.rgba);
        app.set_local_video(slint::Image::from_rgba8(buffer));
        app.set_has_local_video(true);
    }
    if !remotes.is_empty() {
        let existing = app.get_remote_videos();
        let mut rows = (0..existing.row_count())
            .filter_map(|index| existing.row_data(index))
            .collect::<Vec<_>>();
        for (peer_id, frame) in remotes {
            let mut buffer =
                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(frame.width, frame.height);
            buffer.make_mut_bytes().copy_from_slice(&frame.rgba);
            let row = RemoteVideoRow {
                peer_id: peer_id.clone().into(),
                label: format!("Remoto {}", short_id(&peer_id)).into(),
                frame: slint::Image::from_rgba8(buffer),
            };
            if let Some(existing) = rows
                .iter_mut()
                .find(|existing| existing.peer_id == row.peer_id)
            {
                *existing = row;
            } else {
                rows.push(row);
            }
        }
        if rows.len() > MAX_RENDERED_REMOTE_VIDEOS {
            let excess = rows.len() - MAX_RENDERED_REMOTE_VIDEOS;
            rows.drain(..excess);
        }
        app.set_remote_videos(ModelRc::new(VecModel::from(rows)));
        app.set_has_remote_video(true);
    }

    let should_schedule = dispatcher.pending.lock().ok().is_some_and(|pending| {
        (pending.local.is_some() || !pending.remotes.is_empty()) && !pending.scheduled
    });
    if should_schedule {
        schedule_video_ui(&app.as_weak(), dispatcher);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_call_command(
    command: CallCommand,
    app: &slint::Weak<AppWindow>,
    identity: &DeviceIdentity,
    store: &LocalStore,
    discovery: &DiscoveryService,
    connected_peers: &HashSet<libp2p::PeerId>,
    active_call: &mut Option<(uuid::Uuid, uuid::Uuid)>,
    call_engine: &mut Option<CallEngine>,
    sequence: &mut u64,
    local_metrics: &NodeMetrics,
) {
    match command {
        CallCommand::Join {
            community_id,
            call_id,
            input_device,
            output_device,
            video_device,
        } => match CallEngine::with_devices(
            input_device.as_deref(),
            output_device.as_deref(),
            video_device.as_deref(),
        ) {
            Ok(engine) => {
                let mut engine = engine;
                if let Ok(secret) = community_media_secret(store, community_id) {
                    engine.set_media_secret(call_id, &secret);
                }
                let capabilities = engine.local_capabilities_payload();
                if let Some(engine) = call_engine.as_mut() {
                    let _ = Box::pin(engine.close()).await;
                }
                *call_engine = Some(engine);
                *active_call = Some((community_id, call_id));
                let app_weak = app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_call_starting(false);
                        app.set_call_active(true);
                    }
                });
                for peer_id in connected_peers
                    .iter()
                    .filter(|peer_id| is_authorized_peer(store, community_id, **peer_id))
                {
                    let result = send_call_signal(
                        discovery,
                        identity,
                        *peer_id,
                        community_id,
                        call_id,
                        sequence,
                        CallSignalKind::Capabilities,
                        capabilities.clone(),
                    )
                    .await;
                    if let Err(error) = result {
                        update_call_status(app, format!("Falha ao sinalizar capacidades: {error}"));
                    }
                    let result = send_call_signal(
                        discovery,
                        identity,
                        *peer_id,
                        community_id,
                        call_id,
                        sequence,
                        CallSignalKind::SfuMetrics,
                        local_metrics.signal_payload(),
                    )
                    .await;
                    if let Err(error) = result {
                        update_call_status(app, format!("Falha ao sinalizar metricas: {error}"));
                    }
                    let result = send_call_signal(
                        discovery,
                        identity,
                        *peer_id,
                        community_id,
                        call_id,
                        sequence,
                        CallSignalKind::ParticipantState,
                        "join".to_owned(),
                    )
                    .await;
                    if let Err(error) = result {
                        update_call_status(app, format!("Falha ao sinalizar entrada: {error}"));
                    }
                }
                update_call_status(app, "Na voz, aguardando pessoas".to_owned());
            }
            Err(error) => {
                update_call_status(app, format!("Audio indisponivel: {error}"));
                let app_weak = app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_call_active(false);
                        app.set_call_starting(false);
                        app.set_call_muted(false);
                        app.set_has_local_video(false);
                        app.set_has_remote_video(false);
                        app.set_remote_videos(ModelRc::new(VecModel::<RemoteVideoRow>::default()));
                        app.set_screen_sharing(false);
                        app.set_call_status("Fora da voz".into());
                    }
                });
            }
        },
        CallCommand::SetMuted(muted) => {
            if let Some(engine) = call_engine.as_mut() {
                engine.set_muted(muted);
                update_call_status(
                    app,
                    if muted {
                        "Microfone silenciado".to_owned()
                    } else {
                        "Microfone ativo".to_owned()
                    },
                );
            }
        }
        CallCommand::SelectInput(device_id) => {
            if let Some(engine) = call_engine.as_mut() {
                let id = (!device_id.is_empty()).then_some(device_id.as_str());
                match engine.select_input(id) {
                    Ok(()) => update_call_status(app, "Microfone trocado".to_owned()),
                    Err(error) => {
                        update_call_status(app, format!("Falha ao trocar microfone: {error}"));
                    }
                }
            }
        }
        CallCommand::SelectOutput(device_id) => {
            if let Some(engine) = call_engine.as_mut() {
                let id = (!device_id.is_empty()).then_some(device_id.as_str());
                match engine.select_output(id) {
                    Ok(()) => update_call_status(app, "Alto-falante trocado".to_owned()),
                    Err(error) => {
                        update_call_status(app, format!("Falha ao trocar alto-falante: {error}"));
                    }
                }
            }
        }
        CallCommand::SelectVideo(device_id) => {
            if let Some(engine) = call_engine.as_mut() {
                let id = (!device_id.is_empty()).then_some(device_id.as_str());
                match engine.select_video(id) {
                    Ok(()) => update_call_status(app, "Camera trocada".to_owned()),
                    Err(error) => {
                        update_call_status(app, format!("Falha ao trocar camera: {error}"));
                    }
                }
            }
        }
        CallCommand::SetVideoEnabled(enabled) => {
            if let Some(engine) = call_engine.as_mut() {
                engine.set_video_enabled(enabled);
                update_call_status(
                    app,
                    if enabled {
                        "Camera ativada".to_owned()
                    } else {
                        "Camera desativada".to_owned()
                    },
                );
            } else {
                update_call_status(app, "A chamada já foi encerrada".to_owned());
                let app_weak = app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_video_enabled(!enabled);
                        app.set_has_local_video(false);
                    }
                });
            }
        }
        CallCommand::SetScreenSharing(sharing) => {
            if let Some(engine) = call_engine.as_mut() {
                match engine.set_screen_sharing(sharing) {
                    Ok(()) => update_call_status(
                        app,
                        if sharing {
                            "Compartilhando tela".to_owned()
                        } else {
                            "Compartilhamento de tela encerrado".to_owned()
                        },
                    ),
                    Err(error) => {
                        update_call_status(app, format!("Falha ao compartilhar tela: {error}"));
                        let app_weak = app.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(app) = app_weak.upgrade() {
                                app.set_screen_sharing(!sharing);
                            }
                        });
                    }
                }
            } else {
                update_call_status(app, "A chamada já foi encerrada".to_owned());
                let app_weak = app.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(app) = app_weak.upgrade() {
                        app.set_screen_sharing(!sharing);
                    }
                });
            }
        }
        CallCommand::Leave => {
            if let Some((community_id, call_id)) = *active_call {
                for peer_id in connected_peers
                    .iter()
                    .filter(|peer_id| is_authorized_peer(store, community_id, **peer_id))
                {
                    let _ = send_call_signal(
                        discovery,
                        identity,
                        *peer_id,
                        community_id,
                        call_id,
                        sequence,
                        CallSignalKind::Leave,
                        String::new(),
                    )
                    .await;
                }
            }
            if let Some(engine) = call_engine.as_mut() {
                let _ = Box::pin(engine.close()).await;
            }
            *call_engine = None;
            *active_call = None;
            let app_weak = app.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(app) = app_weak.upgrade() {
                    app.set_has_local_video(false);
                    app.set_has_remote_video(false);
                    app.set_remote_videos(ModelRc::new(VecModel::<RemoteVideoRow>::default()));
                    app.set_call_starting(false);
                    app.set_call_active(false);
                    app.set_screen_sharing(false);
                }
            });
            update_call_status(app, "Fora da voz".to_owned());
        }
    }
}

fn direct_session_metadata_key(community_id: uuid::Uuid, peer_key: &[u8; 32]) -> String {
    format!("direct-session:{community_id}:{}", BASE64.encode(peer_key))
}

fn save_direct_session(
    store: &LocalStore,
    community_id: uuid::Uuid,
    peer_key: &[u8; 32],
    session: &DoubleRatchetSession,
) -> Result<()> {
    let value = serde_json::to_string(&session.state())?;
    store.set_metadata(&direct_session_metadata_key(community_id, peer_key), &value)?;
    Ok(())
}

fn load_direct_session(
    store: &LocalStore,
    community_id: uuid::Uuid,
    peer_key: &[u8; 32],
) -> Result<Option<DoubleRatchetSession>> {
    let Some(value) = store.get_metadata(&direct_session_metadata_key(community_id, peer_key))?
    else {
        return Ok(None);
    };
    let state = serde_json::from_str::<DoubleRatchetState>(&value)
        .context("estado da sessão direta inválido")?;
    Ok(Some(DoubleRatchetSession::from_state(&state)))
}

fn handle_direct_signal(
    peer_id: libp2p::PeerId,
    signal: &CallSignal,
    identity: &DeviceIdentity,
    store: &LocalStore,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
) -> Result<String> {
    let local_key = identity.public_key_bytes();
    if peer_id_for_device_key(&signal.author_key) != Some(peer_id)
        || signal.author_key == local_key
        || !store.is_authorized_member(signal.community_id, &signal.author_key)?
        || !store.is_authorized_member(signal.community_id, &local_key)?
    {
        anyhow::bail!("origem da mensagem direta não autorizada");
    }
    let conversation_id = direct_conversation_id(signal.community_id, local_key, signal.author_key);
    if signal.call_id != conversation_id {
        anyhow::bail!("conversa direta inválida");
    }
    let key = (signal.community_id, signal.author_key);
    match signal.kind {
        CallSignalKind::DirectSessionHello => {
            let hello = serde_json::from_str::<DirectSessionHello>(&signal.payload)
                .context("hello da sessão direta inválido")?;
            if hello.version != 1
                || hello.conversation_id != conversation_id
                || hello.dh_public_key == local_key
            {
                anyhow::bail!("hello da sessão direta inválido");
            }
            let secret = community_media_secret(store, signal.community_id)?;
            let session = DoubleRatchetSession::initialize_responder(
                secret,
                derive_initial_private(secret, local_key),
            );
            save_direct_session(store, signal.community_id, &signal.author_key, &session)?;
            direct_sessions.insert(key, session);
            Ok(format!(
                "Sessão direta pronta com {}",
                short_id(&hex_prefix(&signal.author_key))
            ))
        }
        CallSignalKind::DirectMessage => {
            let envelope = serde_json::from_str::<DirectMessageEnvelope>(&signal.payload)
                .context("envelope da mensagem direta inválido")?;
            envelope
                .verify(current_timestamp())
                .context("assinatura da mensagem direta inválida")?;
            if envelope.community_id != signal.community_id
                || envelope.conversation_id != conversation_id
                || envelope.sender_key != signal.author_key
                || envelope.recipient_key != local_key
            {
                anyhow::bail!("destino da mensagem direta inválido");
            }
            let mut session = if let Some(session) = direct_sessions.remove(&key) {
                session
            } else if let Some(session) =
                load_direct_session(store, signal.community_id, &signal.author_key)?
            {
                session
            } else {
                let secret = community_media_secret(store, signal.community_id)?;
                DoubleRatchetSession::initialize_responder(
                    secret,
                    derive_initial_private(secret, local_key),
                )
            };
            let body = String::from_utf8(session.decrypt(&envelope.ratchet)?)
                .context("corpo da mensagem direta não é UTF-8")?;
            if body.len() > 16 * 1024 {
                anyhow::bail!("mensagem direta excede 16 KiB");
            }
            store.record_direct_message(&envelope, &body, &local_key, current_timestamp())?;
            save_direct_session(store, signal.community_id, &signal.author_key, &session)?;
            direct_sessions.insert(key, session);
            Ok(format!(
                "Mensagem direta recebida de {}",
                short_id(&hex_prefix(&signal.author_key))
            ))
        }
        _ => anyhow::bail!("sinal direto inesperado"),
    }
}

async fn bootstrap_direct_sessions(
    peer_id: libp2p::PeerId,
    identity: &DeviceIdentity,
    store: &LocalStore,
    discovery: &DiscoveryService,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
    sequence: &mut u64,
) -> Result<()> {
    let local_key = identity.public_key_bytes();
    let Some(remote_key) = device_key_for_peer_id(store, peer_id)? else {
        return Ok(());
    };
    if local_key >= remote_key {
        return Ok(());
    }
    for community in store.communities()? {
        if !store.is_authorized_member(community.id, &remote_key)? {
            continue;
        }
        let conversation_id = direct_conversation_id(community.id, local_key, remote_key);
        let key = (community.id, remote_key);
        if direct_sessions.contains_key(&key)
            || load_direct_session(store, community.id, &remote_key)?.is_some()
        {
            continue;
        }
        let secret = community_media_secret(store, community.id)?;
        let remote_private = derive_initial_private(secret, remote_key);
        let session = DoubleRatchetSession::initialize_initiator(
            secret,
            nexo_core::public_key_from_private(remote_private),
        );
        let hello = serde_json::to_string(&DirectSessionHello::new(
            conversation_id,
            session.dh_public_key(),
        ))?;
        send_call_signal(
            discovery,
            identity,
            peer_id,
            community.id,
            conversation_id,
            sequence,
            CallSignalKind::DirectSessionHello,
            hello,
        )
        .await?;
        save_direct_session(store, community.id, &remote_key, &session)?;
        direct_sessions.insert(key, session);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_direct_command(
    community_id: uuid::Uuid,
    recipient_key: [u8; 32],
    body: String,
    app: &slint::Weak<AppWindow>,
    identity: &DeviceIdentity,
    store: &LocalStore,
    discovery: &DiscoveryService,
    connected_peers: &HashSet<libp2p::PeerId>,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
    sequence: &mut u64,
) -> String {
    if body.trim().is_empty() {
        return "Mensagem direta vazia".to_owned();
    }
    if body.len() > 16 * 1024 {
        return "Mensagem direta excede 16 KiB".to_owned();
    }
    let local_key = identity.public_key_bytes();
    if local_key == recipient_key {
        return "Não é possível enviar mensagem para si mesmo".to_owned();
    }
    if !store
        .is_authorized_member(community_id, &recipient_key)
        .unwrap_or(false)
    {
        return "Destinatário não pertence à comunidade".to_owned();
    }
    let Some(peer_id) = peer_id_for_device_key(&recipient_key) else {
        return "Chave do destinatário inválida".to_owned();
    };
    let connected = connected_peers.contains(&peer_id);
    let conversation_id = direct_conversation_id(community_id, local_key, recipient_key);
    let key = (community_id, recipient_key);
    let mut live_delivered = false;
    let mut session = match direct_sessions.remove(&key) {
        Some(session) => session,
        None => match load_direct_session(store, community_id, &recipient_key) {
            Ok(Some(session)) => session,
            Ok(None) => {
                let Ok(secret) = community_media_secret(store, community_id) else {
                    return "A comunidade não tem segredo de mídia disponível".to_owned();
                };
                let remote_private = derive_initial_private(secret, recipient_key);
                let session = DoubleRatchetSession::initialize_initiator(
                    secret,
                    nexo_core::public_key_from_private(remote_private),
                );
                let hello = match serde_json::to_string(&DirectSessionHello::new(
                    conversation_id,
                    session.dh_public_key(),
                )) {
                    Ok(hello) => hello,
                    Err(error) => return format!("Falha ao preparar sessão direta: {error}"),
                };
                if connected {
                    let _ = send_call_signal(
                        discovery,
                        identity,
                        peer_id,
                        community_id,
                        conversation_id,
                        sequence,
                        CallSignalKind::DirectSessionHello,
                        hello,
                    )
                    .await;
                }
                session
            }
            Err(error) => return format!("Falha ao carregar sessão direta: {error}"),
        },
    };
    if !session.can_encrypt() {
        let Ok(secret) = community_media_secret(store, community_id) else {
            direct_sessions.insert(key, session);
            return "A comunidade não tem segredo de mídia disponível".to_owned();
        };
        let remote_private = derive_initial_private(secret, recipient_key);
        session = DoubleRatchetSession::initialize_initiator(
            secret,
            nexo_core::public_key_from_private(remote_private),
        );
        let hello = match serde_json::to_string(&DirectSessionHello::new(
            conversation_id,
            session.dh_public_key(),
        )) {
            Ok(hello) => hello,
            Err(error) => return format!("Falha ao preparar sessão direta: {error}"),
        };
        if connected {
            let _ = send_call_signal(
                discovery,
                identity,
                peer_id,
                community_id,
                conversation_id,
                sequence,
                CallSignalKind::DirectSessionHello,
                hello,
            )
            .await;
        }
    }
    let ratchet = session.encrypt(body.as_bytes());
    let envelope = match DirectMessageEnvelope::create(
        identity,
        community_id,
        conversation_id,
        recipient_key,
        ratchet,
        current_timestamp(),
    ) {
        Ok(envelope) => envelope,
        Err(error) => return format!("Falha ao assinar mensagem direta: {error}"),
    };
    let payload = match serde_json::to_string(&envelope) {
        Ok(payload) => payload,
        Err(error) => return format!("Falha ao serializar mensagem direta: {error}"),
    };
    if connected {
        live_delivered = send_call_signal(
            discovery,
            identity,
            peer_id,
            community_id,
            conversation_id,
            sequence,
            CallSignalKind::DirectMessage,
            payload,
        )
        .await
        .is_ok();
    }
    if let Err(error) =
        store.record_direct_message(&envelope, &body, &local_key, current_timestamp())
    {
        update_status(
            app,
            format!("Mensagem enviada, mas não foi salva localmente: {error}"),
        );
    }
    if let Err(error) = save_direct_session(store, community_id, &recipient_key, &session) {
        update_status(
            app,
            format!("Mensagem enviada, mas a sessão não foi salva: {error}"),
        );
    }
    direct_sessions.insert(key, session);
    if live_delivered {
        "Mensagem direta enviada".to_owned()
    } else {
        "Mensagem direta salva; será sincronizada quando o dispositivo voltar à rede".to_owned()
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn handle_call_signals(
    peer_id: libp2p::PeerId,
    request: SignalRequest,
    app: &slint::Weak<AppWindow>,
    identity: &DeviceIdentity,
    store: &LocalStore,
    discovery: &DiscoveryService,
    active_call: Option<(uuid::Uuid, uuid::Uuid)>,
    call_engine: &mut Option<CallEngine>,
    sequence: &mut u64,
    sfu_metrics: &mut HashMap<String, NodeMetrics>,
    sfu_topology: &mut SfuTopology,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
) -> String {
    let peer_name = peer_id.to_string();
    for signal in request.signals {
        if signal.author_key != request.device_key {
            continue;
        }
        let accepted = store
            .accept_call_signal(&signal, current_timestamp())
            .unwrap_or(false);
        if !accepted {
            continue;
        }
        if matches!(
            signal.kind,
            CallSignalKind::DirectSessionHello | CallSignalKind::DirectMessage
        ) {
            match handle_direct_signal(peer_id, &signal, identity, store, direct_sessions) {
                Ok(status) => update_status(app, status),
                Err(error) => update_status(app, format!("Mensagem direta recusada: {error}")),
            }
            continue;
        }
        let Some((community_id, call_id)) = active_call else {
            continue;
        };
        if signal.community_id != community_id || signal.call_id != call_id {
            continue;
        }
        let Some(engine) = call_engine.as_mut() else {
            continue;
        };
        let result: Result<()> = Box::pin(async {
            match signal.kind {
                CallSignalKind::ParticipantState => {
                    if signal.payload == "join" {
                        send_call_signal(
                            discovery,
                            identity,
                            peer_id,
                            community_id,
                            call_id,
                            sequence,
                            CallSignalKind::ParticipantState,
                            "present".to_owned(),
                        )
                        .await?;
                    }
                    if call_negotiation_role(&identity.public_key_bytes(), &signal.author_key)
                        == Some(CallNegotiationRole::Offerer)
                        && !engine.has_peer(&peer_name, call_id)
                    {
                        let offer =
                            Box::pin(engine.create_offer(peer_name.clone(), call_id)).await?;
                        let codec = engine
                            .peer_video_codec(&peer_name, call_id)
                            .unwrap_or(VideoCodec::Vp8);
                        let offer = encode_call_offer(codec, offer)?;
                        send_call_signal(
                            discovery,
                            identity,
                            peer_id,
                            community_id,
                            call_id,
                            sequence,
                            CallSignalKind::Offer,
                            offer,
                        )
                        .await?;
                    }
                    Ok(())
                }
                CallSignalKind::Capabilities => {
                    engine.set_peer_capabilities(&peer_name, &signal.payload);
                    Ok(())
                }
                CallSignalKind::SfuMetrics => {
                    let metrics = NodeMetrics::from_signal_payload(&peer_name, &signal.payload)
                        .ok_or_else(|| anyhow::anyhow!("metricas SFU invalidas"))?;
                    sfu_metrics.insert(peer_name.clone(), metrics);
                    sfu_topology.record_heartbeat(&peer_name, current_timestamp());
                    Ok(())
                }
                CallSignalKind::SfuHeartbeat => {
                    sfu_topology.record_heartbeat(&peer_name, current_timestamp());
                    Ok(())
                }
                CallSignalKind::SfuMigration => {
                    let proposal = SfuMigrationProposal::from_signal_payload(&signal.payload)
                        .ok_or_else(|| anyhow::anyhow!("proposta de migracao SFU invalida"))?;
                    if proposal.from != peer_name {
                        anyhow::bail!("proposta de migracao SFU nao veio do relay atual");
                    }
                    if !engine.has_peer(&proposal.to, call_id) {
                        anyhow::bail!("destino da migracao SFU nao participa da chamada");
                    }
                    sfu_topology.accept_migration(&proposal, current_timestamp());
                    Ok(())
                }
                CallSignalKind::Offer => {
                    if call_negotiation_role(&identity.public_key_bytes(), &signal.author_key)
                        != Some(CallNegotiationRole::Answerer)
                    {
                        anyhow::bail!("oferta recebida do lado incorreto da negociacao");
                    }
                    let (codec, offer) = decode_call_offer(&signal.payload)?;
                    let answer = match codec {
                        Some(codec) => {
                            Box::pin(engine.accept_offer_with_codec(
                                peer_name.clone(),
                                call_id,
                                offer,
                                codec,
                            ))
                            .await?
                        }
                        None => {
                            Box::pin(engine.accept_offer(peer_name.clone(), call_id, offer)).await?
                        }
                    };
                    send_call_signal(
                        discovery,
                        identity,
                        peer_id,
                        community_id,
                        call_id,
                        sequence,
                        CallSignalKind::Answer,
                        answer,
                    )
                    .await
                }
                CallSignalKind::Answer => {
                    if call_negotiation_role(&identity.public_key_bytes(), &signal.author_key)
                        != Some(CallNegotiationRole::Offerer)
                    {
                        anyhow::bail!("resposta recebida do lado incorreto da negociacao");
                    }
                    engine
                        .accept_answer(&peer_name, call_id, signal.payload)
                        .await?;
                    Ok(())
                }
                CallSignalKind::IceCandidate
                | CallSignalKind::DirectSessionHello
                | CallSignalKind::DirectMessage => Ok(()),
                CallSignalKind::Leave => {
                    Box::pin(engine.remove_peer(&peer_name)).await?;
                    Ok(())
                }
            }
        })
        .await;
        if let Err(error) = result {
            update_call_status(app, format!("Falha ao conectar voz: {error}"));
        }
    }
    format!(
        "Voz direta com {} pessoa(s)",
        call_engine
            .as_ref()
            .map_or(0, CallEngine::connected_peer_count)
    )
}

#[allow(clippy::too_many_arguments)]
async fn send_call_signal(
    discovery: &DiscoveryService,
    identity: &DeviceIdentity,
    peer_id: libp2p::PeerId,
    community_id: uuid::Uuid,
    call_id: uuid::Uuid,
    sequence: &mut u64,
    kind: CallSignalKind,
    payload: String,
) -> Result<()> {
    *sequence = sequence.saturating_add(1);
    let signal = CallSignal::create(
        identity,
        community_id,
        call_id,
        *sequence,
        kind,
        payload,
        current_timestamp(),
    )?;
    discovery
        .signal_peer(
            peer_id,
            SignalRequest::new(identity.public_key_bytes(), vec![signal]),
        )
        .await
}

fn is_authorized_peer(
    store: &LocalStore,
    community_id: uuid::Uuid,
    peer_id: libp2p::PeerId,
) -> bool {
    store.authorized_members(community_id).is_ok_and(|members| {
        members
            .iter()
            .filter_map(peer_id_for_device_key)
            .any(|authorized| authorized == peer_id)
    })
}

fn peer_id_for_device_key(public_key: &[u8; 32]) -> Option<libp2p::PeerId> {
    let public_key = libp2p::identity::ed25519::PublicKey::try_from_bytes(public_key).ok()?;
    Some(libp2p::identity::PublicKey::from(public_key).to_peer_id())
}

fn device_key_for_peer_id(store: &LocalStore, peer_id: libp2p::PeerId) -> Result<Option<[u8; 32]>> {
    for community in store.communities()? {
        for key in store.authorized_members(community.id)? {
            if peer_id_for_device_key(&key) == Some(peer_id) {
                return Ok(Some(key));
            }
        }
    }
    Ok(None)
}

async fn publish_sync_tokens(discovery: &DiscoveryService, store: &LocalStore) -> Result<()> {
    discovery
        .update_communities(
            store.database_epoch()?,
            store
                .sync_tokens()?
                .into_iter()
                .map(|(_, token)| token)
                .collect(),
        )
        .await
}

async fn revoke_member_from_community(
    store: &LocalStore,
    identity: &DeviceIdentity,
    discovery: &DiscoveryService,
    connected_peers: &HashSet<libp2p::PeerId>,
    community_id: uuid::Uuid,
    member_key: [u8; 32],
) -> Result<String> {
    let own_key = identity.public_key_bytes();
    if !is_local_founder(store, community_id, &own_key)? {
        anyhow::bail!("somente o fundador pode remover membros");
    }
    if member_key == own_key {
        anyhow::bail!("o fundador nao pode remover a propria identidade");
    }
    if !store.is_authorized_member(community_id, &member_key)? {
        anyhow::bail!("membro ja nao esta autorizado");
    }
    let mut group = ensure_local_mls_state(store, community_id)?;
    let leaf_index = group
        .members
        .iter()
        .find(|member| member.public_key == member_key)
        .map(|member| member.leaf_index)
        .context("membro nao esta no estado de associacao local")?;
    let commit = MlsCommit::create_remove(identity, &group, leaf_index)?;
    group.apply_commit_for_identity(&commit, identity)?;
    store.save_mls_commit(&commit)?;
    store.revoke_member(community_id, &member_key)?;
    store.save_mls_group(&group)?;

    if let Ok(tokens) = store.sync_tokens() {
        let offer = SyncRequest::offer(
            own_key,
            tokens.into_iter().map(|(_, token)| token).collect(),
        );
        for peer_id in connected_peers {
            let _ = discovery.sync_peer(*peer_id, offer.clone()).await;
        }
    }
    Ok(format!(
        "Membro {} removido; commit sincronizando",
        short_id(&hex_prefix(&member_key))
    ))
}

fn remember_listen_address(
    shared: &Arc<Mutex<Vec<String>>>,
    address: &libp2p::Multiaddr,
    peer_id: libp2p::PeerId,
) {
    if address
        .iter()
        .any(|protocol| matches!(protocol, libp2p::multiaddr::Protocol::P2pCircuit))
    {
        if let Ok(mut addresses) = shared.lock() {
            addresses.push(address.to_string());
            addresses.sort();
            addresses.dedup();
            addresses.truncate(8);
        }
        return;
    }
    let Some(port) = address.iter().find_map(|protocol| match protocol {
        libp2p::multiaddr::Protocol::Tcp(port) => Some(("tcp", port)),
        libp2p::multiaddr::Protocol::Udp(port) => Some(("quic", port)),
        _ => None,
    }) else {
        return;
    };
    let mut reachable = HashSet::new();
    if let Ok(interfaces) = if_addrs::get_if_addrs() {
        for interface in interfaces {
            let ip = interface.ip();
            if !interface.is_oper_up()
                || interface.is_p2p()
                || interface.is_link_local()
                || ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || is_virtual_interface(&interface.name)
            {
                continue;
            }
            let base = match ip {
                std::net::IpAddr::V4(ip) => format!("/ip4/{ip}"),
                std::net::IpAddr::V6(ip) => format!("/ip6/{ip}"),
            };
            let value = match port {
                ("tcp", port) => format!("{base}/tcp/{port}/p2p/{peer_id}"),
                ("quic", port) => format!("{base}/udp/{port}/quic-v1/p2p/{peer_id}"),
                _ => continue,
            };
            reachable.insert(value);
        }
    }
    if let Ok(mut addresses) = shared.lock() {
        addresses.extend(reachable);
        addresses.sort();
        addresses.dedup();
        addresses.truncate(8);
    }
}

fn is_virtual_interface(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "docker",
        "vethernet",
        "wsl",
        "virtualbox",
        "vmware",
        "loopback",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

#[allow(clippy::too_many_lines)]
fn build_sync_batch(
    store: &LocalStore,
    identity: &DeviceIdentity,
    peer_id: &str,
    remote_device_key: Option<[u8; 32]>,
    receiver_epoch: uuid::Uuid,
    wanted_tokens: &[[u8; 32]],
) -> Result<SyncRequest> {
    let now = current_timestamp();
    let mut communities = Vec::new();
    for (community_id, token) in store.sync_tokens()? {
        if !wanted_tokens.contains(&token) {
            continue;
        }
        let credentials = store.credentials(community_id)?;
        let remote_key = remote_device_key.or_else(|| {
            peer_id
                .parse::<libp2p::PeerId>()
                .ok()
                .and_then(|peer| device_key_for_peer_id(store, peer).ok().flatten())
        });
        let local_authorized =
            store.is_authorized_member(community_id, &identity.public_key_bytes())?;
        let remote_authorized = remote_key
            .map(|key| store.is_authorized_member(community_id, &key))
            .transpose()?
            .unwrap_or(true);
        let share_history = local_authorized && remote_authorized;
        let (messages, messages_have_more) = if share_history {
            store.sync_page(
                peer_id,
                receiver_epoch,
                community_id,
                MAX_MESSAGES_PER_COMMUNITY,
                now,
            )?
        } else {
            (Vec::new(), false)
        };
        store.record_pending(
            peer_id,
            receiver_epoch,
            community_id,
            &messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
        )?;
        let (direct_messages, direct_have_more) = if share_history {
            if let Some(remote_key) = remote_key {
                let (direct_messages, direct_have_more) = store.sync_direct_page(
                    peer_id,
                    receiver_epoch,
                    community_id,
                    &remote_key,
                    MAX_DIRECT_MESSAGES_PER_COMMUNITY,
                )?;
                store.record_pending_direct(
                    peer_id,
                    receiver_epoch,
                    community_id,
                    &direct_messages
                        .iter()
                        .map(|message| message.id)
                        .collect::<Vec<_>>(),
                )?;
                (direct_messages, direct_have_more)
            } else {
                (Vec::new(), false)
            }
        } else {
            (Vec::new(), false)
        };
        let (mls_commits, mls_have_more) = store.sync_mls_page(
            peer_id,
            receiver_epoch,
            community_id,
            MAX_MLS_COMMITS_PER_COMMUNITY,
        )?;
        store.record_pending(
            peer_id,
            receiver_epoch,
            community_id,
            &mls_commits
                .iter()
                .map(|commit| commit.id)
                .collect::<Vec<_>>(),
        )?;
        communities.push(CommunitySync {
            community_id,
            credentials,
            channels: store
                .channels(community_id)?
                .into_iter()
                .map(|channel| SyncChannel {
                    id: channel.id,
                    community_id: channel.community_id,
                    name: channel.name,
                    position: channel.position,
                    kind: channel.kind.as_str().to_owned(),
                })
                .collect(),
            messages,
            direct_messages,
            mls_commits,
            has_more: messages_have_more || direct_have_more || mls_have_more,
        });
    }
    Ok(SyncRequest::batch(
        identity.public_key_bytes(),
        receiver_epoch,
        communities,
    ))
}

#[allow(clippy::too_many_lines)]
fn apply_sync_batch(
    store: &LocalStore,
    identity: &DeviceIdentity,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
    request: &SyncRequest,
) -> Result<(usize, uuid::Uuid, Vec<CommunityAck>, Vec<uuid::Uuid>)> {
    if !request.is_within_limits() {
        anyhow::bail!("lote fora dos limites ou versao incompativel");
    }
    let SyncRequest::Batch {
        device_key,
        receiver_epoch,
        communities,
        ..
    } = request
    else {
        return Ok((0, store.database_epoch()?, Vec::new(), Vec::new()));
    };
    let local_epoch = store.database_epoch()?;
    if *receiver_epoch != local_epoch {
        anyhow::bail!(
            "lote destinado a outra base local (recebido={receiver_epoch}, local={local_epoch})"
        );
    }
    let now = current_timestamp();
    let mut inserted = 0;
    let mut acknowledgements = Vec::new();
    let mut changed_communities = Vec::new();
    for community in communities {
        if !community
            .credentials
            .iter()
            .any(|credential| credential.member_key == *device_key)
        {
            continue;
        }
        for credential in &community.credentials {
            if credential.invite.network_id == community.community_id {
                let _ = store.import_credential(credential, now);
            }
        }
        let previous_mls_hash = store
            .mls_group(community.community_id)?
            .map(|state| state.state_hash());
        apply_mls_commits(
            store,
            identity,
            community.community_id,
            &community.mls_commits,
        )?;
        let next_mls_hash = store
            .mls_group(community.community_id)?
            .map(|state| state.state_hash());
        if previous_mls_hash != next_mls_hash {
            changed_communities.push(community.community_id);
        }
        for channel in &community.channels {
            if channel.community_id != community.community_id {
                continue;
            }
            let kind = if channel.kind.eq_ignore_ascii_case("voice") {
                ChannelKind::Voice
            } else {
                ChannelKind::Text
            };
            let _ = store.import_channel(&Channel {
                id: channel.id,
                community_id: channel.community_id,
                name: channel.name.clone(),
                position: channel.position,
                kind,
            });
        }
        let mls_state = store.mls_group(community.community_id)?;
        let (_, new_messages) = store.import_messages_accepted_with_mls(
            community.community_id,
            &community.messages,
            mls_state.as_ref(),
            now,
        )?;
        inserted += new_messages;
        let (new_direct_messages, direct_message_ids) = apply_direct_sync_messages(
            store,
            identity,
            direct_sessions,
            community.community_id,
            &community.direct_messages,
        )?;
        inserted += new_direct_messages;
        acknowledgements.push(CommunityAck {
            community_id: community.community_id,
            processed_message_ids: community
                .messages
                .iter()
                .map(|message| message.id)
                .collect(),
            processed_direct_message_ids: direct_message_ids,
            processed_mls_commit_ids: community
                .mls_commits
                .iter()
                .map(|commit| commit.id)
                .collect(),
            request_next: community.has_more,
        });
    }
    Ok((
        inserted,
        *receiver_epoch,
        acknowledgements,
        changed_communities,
    ))
}

#[allow(clippy::if_not_else, clippy::too_many_lines)]
fn apply_mls_commits(
    store: &LocalStore,
    identity: &DeviceIdentity,
    community_id: uuid::Uuid,
    commits: &[MlsCommit],
) -> Result<usize> {
    let Some(credential) = store.credentials(community_id)?.into_iter().next() else {
        return Ok(0);
    };
    let founder_key = DeviceIdentity::decode_public_key_text(&credential.invite.inviter_key)?;
    let current_state = ensure_local_mls_state(store, community_id)?;
    for commit in commits {
        if commit.group_id != community_id {
            continue;
        }
        if commit.verify_signature().is_err() {
            continue;
        }
        let already_saved = store.has_mls_commit(commit.id)?;
        if !already_saved {
            store.save_mls_commit(commit)?;
        }
    }
    let mut stored_commits = store.mls_commits(community_id)?;
    let has_add_history = stored_commits
        .iter()
        .any(|commit| matches!(commit.operation, MlsCommitOperation::Add { .. }));
    stored_commits.sort_by_key(|commit| (commit.epoch, commit.id));

    // Newer databases can replay from the founder-only state because every
    // join is represented by a signed Add commit. Legacy databases may have
    // materialized members without that history; keep their current state as
    // the replay base until a complete commit chain is available.
    let mut state = if has_add_history {
        if let Some(group_secret) = credential.invite.group_secret_bytes() {
            MlsGroupState::new_with_secret(
                community_id,
                mls_device_id(&founder_key),
                founder_key,
                group_secret,
            )
        } else {
            MlsGroupState::new(community_id, mls_device_id(&founder_key), founder_key)
        }
    } else {
        current_state
    };
    let mut applied = 0;
    for commit in stored_commits {
        if commit.group_id != community_id || commit.verify_signature().is_err() {
            continue;
        }
        match &commit.operation {
            MlsCommitOperation::Add { public_key, .. } => {
                let authorized_target = store.is_authorized_member(community_id, public_key)?
                    || store.is_revoked_member(community_id, public_key)?
                    || *public_key == commit.committer_key;
                if !authorized_target || state.apply_add_proposal(&commit).is_err() {
                    continue;
                }
                applied += 1;
            }
            MlsCommitOperation::Remove { leaf_index } => {
                let removed_key = state
                    .members
                    .iter()
                    .find(|member| member.leaf_index == *leaf_index)
                    .map(|member| member.public_key);
                let mut next = state.clone();
                if next.apply_commit_for_identity(&commit, identity).is_err() {
                    continue;
                }
                state = next;
                if let Some(removed_key) = removed_key {
                    let _ = store.revoke_member(community_id, &removed_key)?;
                }
                applied += 1;
            }
        }
    }
    store.save_mls_group(&state)?;
    Ok(applied)
}

fn apply_direct_sync_messages(
    store: &LocalStore,
    identity: &DeviceIdentity,
    direct_sessions: &mut HashMap<(uuid::Uuid, [u8; 32]), DoubleRatchetSession>,
    community_id: uuid::Uuid,
    envelopes: &[DirectMessageEnvelope],
) -> Result<(usize, Vec<uuid::Uuid>)> {
    let local_key = identity.public_key_bytes();
    let mut ordered = envelopes.to_vec();
    ordered.sort_by_key(|envelope| (envelope.created_at, envelope.id));
    let mut inserted = 0;
    let mut processed = Vec::with_capacity(ordered.len());
    for envelope in ordered {
        processed.push(envelope.id);
        if envelope.community_id != community_id
            || envelope.recipient_key != local_key
            || envelope.sender_key == local_key
            || envelope.conversation_id
                != direct_conversation_id(community_id, envelope.sender_key, envelope.recipient_key)
            || !store.is_authorized_member(community_id, &envelope.sender_key)?
        {
            continue;
        }
        if envelope.verify_signature().is_err() {
            continue;
        }
        let key = (community_id, envelope.sender_key);
        let mut session = if let Some(session) = direct_sessions.remove(&key) {
            session
        } else if let Some(session) =
            load_direct_session(store, community_id, &envelope.sender_key)?
        {
            session
        } else {
            let secret = community_media_secret(store, community_id)?;
            DoubleRatchetSession::initialize_responder(
                secret,
                derive_initial_private(secret, local_key),
            )
        };
        let Ok(body) = session.decrypt(&envelope.ratchet) else {
            direct_sessions.insert(key, session);
            continue;
        };
        let Ok(body) = String::from_utf8(body) else {
            direct_sessions.insert(key, session);
            continue;
        };
        if body.len() > 16 * 1024 {
            direct_sessions.insert(key, session);
            continue;
        }
        if store.record_direct_message(&envelope, &body, &local_key, current_timestamp())? {
            inserted += 1;
        }
        save_direct_session(store, community_id, &envelope.sender_key, &session)?;
        direct_sessions.insert(key, session);
    }
    Ok((inserted, processed))
}

fn handle_sync_ack(
    store: &LocalStore,
    identity: &DeviceIdentity,
    peer_id: &str,
    request: &SyncRequest,
) -> Result<Option<SyncRequest>> {
    let SyncRequest::Ack {
        receiver_epoch,
        communities,
        ..
    } = request
    else {
        return Ok(None);
    };
    let mut next_ids = Vec::new();
    for acknowledgement in communities {
        let acknowledged = store.acknowledge_pending(
            peer_id,
            *receiver_epoch,
            acknowledgement.community_id,
            &acknowledgement.processed_message_ids,
        )?;
        let direct_acknowledged = store.acknowledge_pending_direct(
            peer_id,
            *receiver_epoch,
            acknowledgement.community_id,
            &acknowledgement.processed_direct_message_ids,
        )?;
        let mls_acknowledged = store.acknowledge_pending(
            peer_id,
            *receiver_epoch,
            acknowledgement.community_id,
            &acknowledgement.processed_mls_commit_ids,
        )?;
        if acknowledgement.request_next
            && (acknowledged > 0 || direct_acknowledged > 0 || mls_acknowledged > 0)
        {
            next_ids.push(acknowledgement.community_id);
        }
    }
    if next_ids.is_empty() {
        return Ok(None);
    }
    let tokens = store
        .sync_tokens()?
        .into_iter()
        .filter(|(community_id, _)| next_ids.contains(community_id))
        .map(|(_, token)| token)
        .collect::<Vec<_>>();
    build_sync_batch(store, identity, peer_id, None, *receiver_epoch, &tokens).map(Some)
}

fn start_view_refresh(app: &AppWindow, state: Rc<RefCell<AppState>>) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = app.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            if let Some(app) = weak.upgrade() {
                refresh_or_report(&app, &state.borrow());
            }
        },
    );
    timer
}

fn set_result_status(app: &AppWindow, status: &str) {
    app.set_status_text(status.into());
}

fn refresh_or_report(app: &AppWindow, state: &AppState) {
    if let Err(error) = refresh_view(app, state) {
        set_result_status(app, &format!("Falha ao atualizar a conversa: {error}"));
    }
}

fn update_status(app: &slint::Weak<AppWindow>, status: String) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app.upgrade() {
            app.set_status_text(status.into());
        }
    });
}

fn update_call_status(app: &slint::Weak<AppWindow>, status: String) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(app) = app.upgrade() {
            app.set_call_status(status.into());
        }
    });
}

fn format_time(timestamp: u64) -> String {
    i64::try_from(timestamp)
        .ok()
        .and_then(|seconds| DateTime::from_timestamp(seconds, 0))
        .map_or_else(
            || "--:--".to_owned(),
            |value| value.with_timezone(&Local).format("%H:%M").to_string(),
        )
}

fn mls_device_id(public_key: &[u8; 32]) -> String {
    format!("member-{}", hex_prefix(&public_key[..8]))
}

fn ensure_local_mls_state(store: &LocalStore, community_id: uuid::Uuid) -> Result<MlsGroupState> {
    if let Some(state) = store.mls_group(community_id)? {
        return Ok(state);
    }
    let credential = store
        .credentials(community_id)?
        .into_iter()
        .next()
        .context("credencial da comunidade ausente")?;
    let founder_key = DeviceIdentity::decode_public_key_text(&credential.invite.inviter_key)?;
    let member_keys = store.authorized_members(community_id)?;
    if let Some(group_secret) = credential.invite.group_secret_bytes() {
        Ok(store.ensure_mls_group_with_secret(
            community_id,
            mls_device_id(&founder_key),
            founder_key,
            &member_keys,
            group_secret,
        )?)
    } else {
        Ok(store.ensure_mls_group(
            community_id,
            mls_device_id(&founder_key),
            founder_key,
            &member_keys,
        )?)
    }
}

fn hex_prefix(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(6)
        .fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        })
}

fn short_id(value: &str) -> &str {
    value.get(..12).unwrap_or(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_host_selection_is_deterministic_and_excludes_local_peer() {
        let (is_host, targets) = select_media_targets(
            "peer-b",
            vec![
                "peer-c".to_owned(),
                "peer-a".to_owned(),
                "peer-c".to_owned(),
            ],
        );
        assert!(!is_host);
        assert_eq!(targets, vec!["peer-a"]);

        let (is_host, targets) = select_media_targets("peer-a", vec!["peer-b".to_owned()]);
        assert!(is_host);
        assert_eq!(targets, vec!["peer-b"]);
    }

    #[test]
    fn a_single_participant_is_a_host_without_relay_targets() {
        let (is_host, targets) = select_media_targets("peer-a", Vec::new());
        assert!(is_host);
        assert!(targets.is_empty());
    }

    #[test]
    fn sfu_metrics_are_limited_to_current_call_members() {
        let metric = |node_id: &str| NodeMetrics {
            node_id: node_id.to_owned(),
            available_upload_mbps: 100.0,
            packet_loss_percent: 0.0,
            round_trip_ms: 1.0,
            cpu_headroom_percent: 60.0,
            gpu_headroom_percent: 70.0,
            hardware_encoder: true,
            publicly_reachable: false,
        };
        let mut metrics = HashMap::new();
        metrics.insert("peer-a".to_owned(), metric("peer-a"));
        metrics.insert("peer-b".to_owned(), metric("peer-b"));
        metrics.insert("connected-but-not-in-call".to_owned(), metric("extra"));

        let nodes = metrics_for_call(&["peer-a".to_owned(), "peer-b".to_owned()], &metrics);
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.node_id.as_str())
                .collect::<Vec<_>>(),
            vec!["peer-a", "peer-b"]
        );
    }

    #[test]
    fn call_offer_preserves_the_selected_codec() -> Result<()> {
        for codec in [VideoCodec::Vp8, VideoCodec::H264] {
            let payload = encode_call_offer(codec, "v=0\r\n".to_owned())?;
            let (decoded_codec, sdp) = decode_call_offer(&payload)?;
            assert_eq!(decoded_codec, Some(codec));
            assert_eq!(sdp, "v=0\r\n");
        }
        Ok(())
    }

    #[test]
    fn call_offer_accepts_legacy_unwrapped_sdp() -> Result<()> {
        let (codec, sdp) = decode_call_offer("v=0\r\n")?;
        assert_eq!(codec, None);
        assert_eq!(sdp, "v=0\r\n");
        Ok(())
    }

    #[test]
    fn call_offer_rejects_unknown_codec_and_empty_sdp() {
        let unknown = serde_json::json!({ "codec": "av1", "sdp": "v=0" }).to_string();
        assert!(decode_call_offer(&unknown).is_err());
        let empty = serde_json::json!({ "codec": "vp8", "sdp": " " }).to_string();
        assert!(decode_call_offer(&empty).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn founder_revocation_updates_store_and_membership_state() -> Result<()> {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("nexo-moderation-{unique}.sqlite3"));
        let founder = DeviceIdentity::generate();
        let member = DeviceIdentity::generate();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path)?;
        let invite = NetworkInvite::create(&founder, "Moderacao".to_owned(), Vec::new(), now, 600)?;
        let community = store.create_community(invite.network_id, "Moderacao", now)?;
        let credential = CommunityCredential::claim(&founder, invite.clone(), now)?;
        store.authorize_member(community.id, &founder.public_key_bytes(), now)?;
        store.authorize_member(community.id, &member.public_key_bytes(), now)?;
        store.save_credential(&credential)?;
        let group_secret = invite
            .group_secret_bytes()
            .context("convite deveria possuir segredo de grupo")?;
        store.ensure_mls_group_with_secret(
            community.id,
            mls_device_id(&founder.public_key_bytes()),
            founder.public_key_bytes(),
            &[founder.public_key_bytes()],
            group_secret,
        )?;
        let mut group = store
            .mls_group(community.id)?
            .context("estado de associacao deveria existir")?;
        let add = MlsCommit::create_add(
            &founder,
            &group,
            mls_device_id(&member.public_key_bytes()),
            member.public_key_bytes(),
        )?;
        group.apply_commit(&add)?;
        store.save_mls_commit(&add)?;
        store.save_mls_group(&group)?;

        let discovery = DiscoveryService::start(&founder)?;
        let status = revoke_member_from_community(
            &store,
            &founder,
            &discovery,
            &HashSet::new(),
            community.id,
            member.public_key_bytes(),
        )
        .await?;
        assert!(status.contains("removido"));
        assert!(!store.is_authorized_member(community.id, &member.public_key_bytes())?);
        assert!(store.is_revoked_member(community.id, &member.public_key_bytes())?);
        let saved = store
            .mls_group(community.id)?
            .context("estado de associacao deveria permanecer salvo")?;
        assert!(!saved.contains_member(&member.public_key_bytes()));
        assert_eq!(store.mls_commits(community.id)?.len(), 2);

        drop(discovery);
        drop(store);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn synced_revocation_rekeys_media_and_removes_member() -> Result<()> {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("nexo-sync-revocation-{unique}.sqlite3"));
        let founder = DeviceIdentity::generate();
        let member = DeviceIdentity::generate();
        let now = current_timestamp();
        let mut store = LocalStore::open(&path)?;
        let invite =
            NetworkInvite::create(&founder, "Sincronizacao".to_owned(), Vec::new(), now, 600)?;
        let community = store.create_community(invite.network_id, "Sincronizacao", now)?;
        let credential = CommunityCredential::claim(&founder, invite.clone(), now)?;
        store.save_credential(&credential)?;
        store.authorize_member(community.id, &founder.public_key_bytes(), now)?;
        store.authorize_member(community.id, &member.public_key_bytes(), now)?;
        let group_secret = invite
            .group_secret_bytes()
            .context("convite deveria possuir segredo de grupo")?;
        store.ensure_mls_group_with_secret(
            community.id,
            mls_device_id(&founder.public_key_bytes()),
            founder.public_key_bytes(),
            &[founder.public_key_bytes()],
            group_secret,
        )?;
        let mut group = store
            .mls_group(community.id)?
            .context("estado de associacao deveria existir")?;
        let addition = MlsCommit::create_add(
            &founder,
            &group,
            mls_device_id(&member.public_key_bytes()),
            member.public_key_bytes(),
        )?;
        group.apply_commit(&addition)?;
        store.save_mls_commit(&addition)?;
        store.save_mls_group(&group)?;
        let before = community_media_secret(&store, community.id)?;
        let group = store
            .mls_group(community.id)?
            .context("estado de associacao deveria existir")?;
        let leaf_index = group
            .members
            .iter()
            .find(|entry| entry.public_key == member.public_key_bytes())
            .context("membro deveria estar na arvore")?
            .leaf_index;
        let removal = MlsCommit::create_remove(&founder, &group, leaf_index)?;
        store.save_mls_commit(&removal)?;

        apply_mls_commits(&store, &founder, community.id, &[removal])?;

        let after = community_media_secret(&store, community.id)?;
        assert_ne!(before, after, "a revogacao deveria trocar a chave de midia");
        assert!(!store.is_authorized_member(community.id, &member.public_key_bytes())?);
        assert!(store.is_revoked_member(community.id, &member.public_key_bytes())?);

        drop(store);
        let _ = std::fs::remove_file(path);
        Ok(())
    }

    #[test]
    fn sync_batch_applies_private_removal_to_a_receiver() -> Result<()> {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let source_path = std::env::temp_dir().join(format!("nexo-sync-source-{unique}.sqlite3"));
        let receiver_path =
            std::env::temp_dir().join(format!("nexo-sync-receiver-{unique}.sqlite3"));
        let founder = DeviceIdentity::generate();
        let member = DeviceIdentity::generate();
        let now = current_timestamp();
        let invite = NetworkInvite::create(&founder, "Sync".to_owned(), Vec::new(), now, 600)?;
        let founder_credential = CommunityCredential::claim(&founder, invite.clone(), now)?;
        let member_credential = CommunityCredential::claim(&member, invite.clone(), now)?;

        let mut source = LocalStore::open(&source_path)?;
        let source_community = source.create_community(invite.network_id, "Sync", now)?;
        source.save_credential(&founder_credential)?;
        source.authorize_member(source_community.id, &founder.public_key_bytes(), now)?;
        source.authorize_member(source_community.id, &member.public_key_bytes(), now)?;
        let group_secret = invite
            .group_secret_bytes()
            .context("convite deveria possuir segredo de grupo")?;
        source.ensure_mls_group_with_secret(
            source_community.id,
            mls_device_id(&founder.public_key_bytes()),
            founder.public_key_bytes(),
            &[founder.public_key_bytes()],
            group_secret,
        )?;
        let mut source_group = source
            .mls_group(source_community.id)?
            .context("estado de origem ausente")?;
        let addition = MlsCommit::create_add(
            &founder,
            &source_group,
            mls_device_id(&member.public_key_bytes()),
            member.public_key_bytes(),
        )?;
        source_group.apply_commit(&addition)?;
        let removal = MlsCommit::create_remove(
            &founder,
            &source_group,
            source_group
                .members
                .iter()
                .find(|entry| entry.public_key == member.public_key_bytes())
                .context("membro da origem ausente")?
                .leaf_index,
        )?;
        source.save_mls_commit(&addition)?;
        source.save_mls_commit(&removal)?;

        let mut receiver = LocalStore::open(&receiver_path)?;
        let receiver_community = receiver.create_community(invite.network_id, "Sync", now)?;
        receiver.save_credential(&founder_credential)?;
        receiver.authorize_member(receiver_community.id, &founder.public_key_bytes(), now)?;
        receiver.ensure_mls_group_with_secret(
            receiver_community.id,
            mls_device_id(&founder.public_key_bytes()),
            founder.public_key_bytes(),
            &[founder.public_key_bytes()],
            group_secret,
        )?;
        let receiver_epoch = receiver.database_epoch()?;
        let batch = SyncRequest::batch(
            member.public_key_bytes(),
            receiver_epoch,
            vec![CommunitySync {
                community_id: invite.network_id,
                credentials: vec![founder_credential, member_credential],
                channels: Vec::new(),
                messages: Vec::new(),
                direct_messages: Vec::new(),
                mls_commits: vec![addition, removal],
                has_more: false,
            }],
        );
        let mut direct_sessions = HashMap::new();
        let (_, _, _, changed) =
            apply_sync_batch(&receiver, &member, &mut direct_sessions, &batch)?;

        assert_eq!(changed, vec![invite.network_id]);
        assert!(!receiver.is_authorized_member(invite.network_id, &member.public_key_bytes())?);
        assert!(receiver.is_revoked_member(invite.network_id, &member.public_key_bytes())?);
        assert!(
            !receiver
                .mls_group(invite.network_id)?
                .context("estado do receiver ausente")?
                .contains_member(&member.public_key_bytes())
        );

        drop(receiver);
        drop(source);
        let _ = std::fs::remove_file(source_path);
        let _ = std::fs::remove_file(receiver_path);
        Ok(())
    }

    #[test]
    fn voice_note_writer_emits_a_mono_pcm_wav() {
        let path = std::env::temp_dir().join(format!("nexo-voice-{}.wav", uuid::Uuid::new_v4()));
        write_pcm_wav(&path, &[0.0, 0.5, -0.5]).expect("wav should be written");
        let bytes = std::fs::read(&path).expect("wav should be readable");
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        let data_size: [u8; 4] = bytes[40..44].try_into().expect("data size exists");
        assert_eq!(u32::from_le_bytes(data_size), 6);
        std::fs::remove_file(path).expect("temporary wav should be removed");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)]
    async fn signed_file_round_trips_over_the_webrtc_data_channel() -> Result<()> {
        let unique = uuid::Uuid::new_v4().simple().to_string();
        let dir_a = std::env::temp_dir().join(format!("nexo-webrtc-file-a-{unique}"));
        let dir_b = std::env::temp_dir().join(format!("nexo-webrtc-file-b-{unique}"));
        std::fs::create_dir_all(&dir_a)?;
        std::fs::create_dir_all(&dir_b)?;
        let input_path = dir_a.join("dados.bin");
        let payload = (0..20_000_u32)
            .map(|value| u8::try_from(value % 251).unwrap_or_default())
            .collect::<Vec<_>>();
        std::fs::write(&input_path, &payload)?;

        let identity_a = DeviceIdentity::generate();
        let identity_b = DeviceIdentity::generate();
        let peer_a = peer_id_for_device_key(&identity_a.public_key_bytes())
            .context("peer A deveria ser derivado da identidade")?;
        let peer_b = peer_id_for_device_key(&identity_b.public_key_bytes())
            .context("peer B deveria ser derivado da identidade")?;
        let community_id = uuid::Uuid::new_v4();
        let mut store_a = LocalStore::open(&dir_a.join("a.sqlite3"))?;
        let mut store_b = LocalStore::open(&dir_b.join("b.sqlite3"))?;
        let community_a = store_a.create_community(community_id, "Arquivos WebRTC", 1)?;
        let community_b = store_b.create_community(community_id, "Arquivos WebRTC", 1)?;
        for store in [&mut store_a, &mut store_b] {
            store.authorize_member(community_id, &identity_a.public_key_bytes(), 1)?;
            store.authorize_member(community_id, &identity_b.public_key_bytes(), 1)?;
        }

        let call_id = uuid::Uuid::new_v4();
        let mut engine_a = CallEngine::new()?;
        let mut engine_b = CallEngine::new()?;
        let offer = engine_a.create_offer(peer_b.to_string(), call_id).await?;
        let answer = engine_b
            .accept_offer(peer_a.to_string(), call_id, offer)
            .await?;
        engine_a
            .accept_answer(&peer_b.to_string(), call_id, answer)
            .await?;
        for _ in 0..200 {
            let _ = engine_a.tick().await?;
            let _ = engine_b.tick().await?;
            if engine_a.connected_peer_count() == 1 && engine_b.connected_peer_count() == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::ensure!(
            engine_a.connected_peer_count() == 1,
            "A nao conectou a chamada"
        );
        anyhow::ensure!(
            engine_b.connected_peer_count() == 1,
            "B nao conectou a chamada"
        );

        let connected_peers = HashSet::from([peer_b]);
        let result = send_file_over_webrtc(
            &store_a,
            &identity_a,
            community_id,
            community_a.default_channel_id,
            input_path,
            &connected_peers,
            Some(call_id),
            &engine_a,
        )
        .await?;
        anyhow::ensure!(result.is_some(), "o envio deveria escolher WebRTC");

        let mut incoming_files = HashMap::new();
        let mut received = false;
        for _ in 0..200 {
            let events = engine_b.tick().await?;
            for event in events {
                if let CallEngineEvent::DataMessage { peer_id, data } = event {
                    let status = handle_incoming_webrtc_file_message(
                        &store_b,
                        &mut incoming_files,
                        &dir_b.join("downloads"),
                        &peer_id,
                        &data,
                    )?;
                    received |= status.starts_with("Arquivo recebido via WebRTC");
                }
            }
            if received {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        anyhow::ensure!(received, "B nao concluiu o arquivo via WebRTC");
        let stored = store_b
            .file_transfers_in_channel(community_b.default_channel_id)?
            .into_iter()
            .find(|transfer| transfer.status == "completed")
            .context("transferencia WebRTC nao foi concluida no SQLite")?;
        let received_path = stored
            .local_path
            .context("arquivo recebido nao possui caminho local")?;
        anyhow::ensure!(
            std::fs::read(received_path)? == payload,
            "conteudo recebido divergiu"
        );

        engine_a.close().await?;
        engine_b.close().await?;
        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
        Ok(())
    }
}
