//! Nexo desktop application: Slint shell, identity, store and orchestration.
//!
//! The binary entry point (`main.rs`) is a thin wrapper around [`run`]; tests
//! can construct one or more isolated application instances with [`start_app`].

slint::include_modules!();

pub mod tray;
pub use tray::{TrayAction, TrayState};

use std::{
    cell::RefCell,
    collections::HashSet,
    fmt::Write as _,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    thread,
};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Local};
use nexo_core::{
    CallNegotiationRole, CallSignal, CallSignalKind, CommunityCredential, DeviceIdentity,
    NetworkInvite, SignedMessage, call_negotiation_role, current_timestamp,
};
use nexo_media::{CallEngine, CallEngineEvent};
use nexo_net::{
    CommunityAck, CommunitySync, DiscoveryEvent, DiscoveryService, SignalRequest, SyncRequest,
    sync::MAX_MESSAGES_PER_COMMUNITY,
};
use nexo_store::{Community, LocalStore};
use slint::{ModelRc, SharedString, VecModel};

const HISTORY_LIMIT: usize = 200;

/// One isolated Nexo application instance: its own window, identity, store and
/// network discovery. Dropping the instance shuts the network loop down and
/// releases the local database.
pub struct AppInstance {
    pub window: AppWindow,
    _refresh_timer: slint::Timer,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl AppInstance {
    /// Signal the background discovery loop to stop and release its resources.
    /// The instance also performs this shutdown when dropped.
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.shutdown.take() {
            let _ = sender.send(());
        }
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
    listen_addresses: Arc<Mutex<Vec<String>>>,
    dial_queue: tokio::sync::mpsc::UnboundedSender<String>,
    call_queue: tokio::sync::mpsc::UnboundedSender<CallCommand>,
    call_active: bool,
    call_muted: bool,
    input_devices: Vec<nexo_media::AudioDeviceInfo>,
    output_devices: Vec<nexo_media::AudioDeviceInfo>,
    video_devices: Vec<nexo_video::VideoDeviceInfo>,
    selected_input: Option<String>,
    selected_output: Option<String>,
    selected_video: Option<String>,
    video_enabled: bool,
    screen_sharing: bool,
    participants: Arc<Mutex<Vec<nexo_media::ParticipantStatus>>>,
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

/// Build a full application instance rooted at `data_dir`. The directory holds
/// the persisted identity key and the `SQLite` store.
pub fn start_app(data_dir: &Path) -> Result<AppInstance> {
    let window = AppWindow::new()?;
    let identity = DeviceIdentity::load_or_create(&data_dir.join("identity.key"))?;
    let store = LocalStore::open(&data_dir.join("nexo.sqlite3"))?;
    for community in store.communities()? {
        store.authorize_member(
            community.id,
            &identity.public_key_bytes(),
            current_timestamp(),
        )?;
    }
    let selected = store.communities()?.into_iter().next();
    let listen_addresses = Arc::new(Mutex::new(Vec::new()));
    let (dial_queue, dial_requests) = tokio::sync::mpsc::unbounded_channel();
    let (call_queue, call_requests) = tokio::sync::mpsc::unbounded_channel();
    let (input_devices, output_devices) =
        split_audio_devices(nexo_media::enumerate_audio_devices().unwrap_or_default());
    let video_devices = nexo_video::enumerate_cameras().unwrap_or_default();
    let selected_input = default_device_id(&input_devices);
    let selected_output = default_device_id(&output_devices);
    let selected_video = video_devices.first().map(|d| d.id.clone());
    let participants = Arc::new(Mutex::new(Vec::new()));
    let state = Rc::new(RefCell::new(AppState {
        identity: identity.clone(),
        store,
        selected,
        listen_addresses: Arc::clone(&listen_addresses),
        dial_queue,
        call_queue,
        call_active: false,
        call_muted: false,
        input_devices,
        output_devices,
        video_devices,
        selected_input,
        selected_output,
        selected_video,
        video_enabled: true,
        screen_sharing: false,
        participants: Arc::clone(&participants),
    }));

    window.set_peer_id(format!("Dispositivo {}", short_id(&identity.public_key_text())).into());
    bind_actions(&window, &state);
    refresh_device_catalog(&window, &state.borrow());
    refresh_view(&window, &state.borrow())?;
    let refresh_timer = start_view_refresh(&window, Rc::clone(&state));
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    start_discovery(
        window.as_weak(),
        identity,
        data_dir.join("nexo.sqlite3"),
        listen_addresses,
        dial_requests,
        call_requests,
        participants,
        shutdown_rx,
    );
    Ok(AppInstance {
        window,
        _refresh_timer: refresh_timer,
        shutdown: Some(shutdown_tx),
    })
}

/// Run the desktop application for the default data directory, blocking until
/// the window is closed.
pub fn run() -> Result<()> {
    let data_dir = data_dir()?;
    let mut app = start_app(&data_dir)?;
    let result = app.window.run();
    app.shutdown();
    result.map_err(Into::into)
}

pub fn data_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NEXO_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    let base = dirs::data_local_dir().context("the operating system has no local data folder")?;
    Ok(base.join("Nexo"))
}

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
                refresh_or_report(&app, &state);
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
    let _action_state = Rc::clone(state);
    app.on_attach_file(move || {
        if let Some(app) = weak.upgrade() {
            set_result_status(&app, "Seletor de arquivos P2P pronto");
        }
    });

    let weak = app.as_weak();
    let _action_state = Rc::clone(state);
    app.on_toggle_voice_recording(move || {
        if let Some(app) = weak.upgrade() {
            let is_recording = app.get_is_recording_voice();
            app.set_is_recording_voice(!is_recording);
            if is_recording {
                set_result_status(&app, "Nota de voz enviada");
            } else {
                set_result_status(&app, "Gravando nota de voz...");
            }
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
        let Some(community) = state.selected.as_ref() else {
            return;
        };
        let command = CallCommand::Join {
            community_id: community.id,
            call_id: community.default_channel_id,
            input_device: state.selected_input.clone(),
            output_device: state.selected_output.clone(),
            video_device: state.selected_video.clone(),
        };
        if state.call_queue.send(command).is_ok() {
            state.call_active = true;
            state.call_muted = false;
            if let Some(app) = weak.upgrade() {
                app.set_call_active(true);
                app.set_call_muted(false);
                app.set_call_status("Conectando".into());
            }
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
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_leave_call(move || {
        let mut state = action_state.borrow_mut();
        if state.call_queue.send(CallCommand::Leave).is_ok() {
            state.call_active = false;
            state.call_muted = false;
            if let Some(app) = weak.upgrade() {
                app.set_call_active(false);
                app.set_call_muted(false);
                app.set_call_status("Fora da voz".into());
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_select_input_device(move |display_name| {
        let mut state = action_state.borrow_mut();
        let id = id_for_display(&state.input_devices, display_name.as_str())
            .or_else(|| Some(display_name.to_string()));
        if state.call_active {
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
        if state.call_active {
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
        if state.call_active {
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
        let mut state = action_state.borrow_mut();
        state.video_enabled = !state.video_enabled;
        let enabled = state.video_enabled;
        if state
            .call_queue
            .send(CallCommand::SetVideoEnabled(enabled))
            .is_ok()
            && let Some(app) = weak.upgrade()
        {
            app.set_video_enabled(enabled);
            if !enabled {
                app.set_has_local_video(false);
            }
        }
    });

    let weak = app.as_weak();
    let action_state = Rc::clone(state);
    app.on_toggle_screen_share(move || {
        let mut state = action_state.borrow_mut();
        state.screen_sharing = !state.screen_sharing;
        let sharing = state.screen_sharing;
        if state
            .call_queue
            .send(CallCommand::SetScreenSharing(sharing))
            .is_ok()
            && let Some(app) = weak.upgrade()
        {
            app.set_screen_sharing(sharing);
            if !sharing && !state.video_enabled {
                app.set_has_local_video(false);
            }
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
    for address in &invite.addresses {
        let _ = state.dial_queue.send(address.clone());
    }
    state.selected = Some(community);
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
    state
        .store
        .authorize_member(community.id, &inviter_key, invite.created_at)?;
    state
        .store
        .authorize_member(community.id, &state.identity.public_key_bytes(), now)?;
    state.store.save_credential(&credential)?;
    for address in &invite.addresses {
        let _ = state.dial_queue.send(address.clone());
    }
    state.selected = Some(community);
    Ok(())
}

fn send_message(state: &Rc<RefCell<AppState>>, body: &str) -> Result<()> {
    let state = state.borrow();
    let community = state
        .selected
        .as_ref()
        .context("nenhuma comunidade selecionada")?;
    let now = current_timestamp();
    let processed_body = nexo_core::replace_emoji_shortcodes(body);
    let message = SignedMessage::create(
        &state.identity,
        community.id,
        community.default_channel_id,
        processed_body,
        now,
    )?;
    state.store.insert_message(&message, now)?;
    Ok(())
}

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
        let own_key = state.identity.public_key_bytes();
        let rows = state
            .store
            .messages(
                community.default_channel_id,
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
                body: message.body.into(),
                time: format_time(message.created_at).into(),
                mine: message.author_key == own_key,
            })
            .collect::<Vec<_>>();
        app.set_messages(ModelRc::new(VecModel::from(rows)));
    } else {
        app.set_has_community(false);
        app.set_active_community(SharedString::new());
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
    mut dial_requests: tokio::sync::mpsc::UnboundedReceiver<String>,
    mut call_requests: tokio::sync::mpsc::UnboundedReceiver<CallCommand>,
    participants: Arc<Mutex<Vec<nexo_media::ParticipantStatus>>>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
) {
    thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                update_status(&app, format!("Falha ao iniciar rede: {error}"));
                return;
            }
        };
        runtime.block_on(async move {
            let store = match LocalStore::open(&database_path) {
                Ok(store) => store,
                Err(error) => {
                    update_status(&app, format!("Sincronizacao local indisponivel: {error}"));
                    return;
                }
            };
            let mut discovery = match DiscoveryService::start(&identity) {
                Ok(discovery) => discovery,
                Err(error) => {
                    update_status(&app, format!("Descoberta local indisponivel: {error}"));
                    return;
                }
            };
            if let Err(error) = publish_sync_tokens(&discovery, &store).await {
                update_status(&app, format!("Falha ao preparar sincronizacao: {error}"));
            }
            update_status(&app, "Preparando rede local".to_owned());
            let mut sync_interval = tokio::time::interval(std::time::Duration::from_secs(5));
            let mut media_interval = tokio::time::interval(std::time::Duration::from_millis(10));
            let mut connected_peers = HashSet::new();
            let mut active_call: Option<(uuid::Uuid, uuid::Uuid)> = None;
            let mut call_engine: Option<CallEngine> = None;
            let mut signal_sequence = 0_u64;
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
                            )).await;
                        }
                        continue;
                    }
                    _ = media_interval.tick() => {
                        if let Some(engine) = call_engine.as_mut() {
                            let states = engine.participant_status();
                            if let Ok(mut shared) = participants.lock() {
                                *shared = states;
                            }
                            match engine.tick().await {
                                Ok(events) => {
                                    for event in &events {
                                        match event {
                                            CallEngineEvent::LocalVideoFrame { width, height, rgba } => {
                                                update_local_video(&app, *width, *height, rgba.clone());
                                            }
                                            CallEngineEvent::RemoteVideoFrame { width, height, rgba, .. } => {
                                                update_remote_video(&app, *width, *height, rgba.clone());
                                            }
                                            _ => {}
                                        }
                                    }
                                    if !events.is_empty() {
                                        update_call_status(&app, call_engine_status(&events, engine));
                                    }
                                }
                                Err(error) => update_call_status(&app, format!("Falha no audio/video: {error}")),
                            }
                        } else if let Ok(mut shared) = participants.lock() {
                            shared.clear();
                        }
                        continue;
                    }
                };
                let status = match event {
                    DiscoveryEvent::Listening(address) => {
                        remember_listen_address(
                            &listen_addresses,
                            &address,
                            discovery.local_peer_id(),
                        );
                        "Rede local ativa".to_owned()
                    }
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
                        if let Some((community_id, call_id)) = active_call
                            && is_authorized_peer(&store, community_id, peer_id)
                        {
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
                        receiver_epoch,
                        tokens,
                    } => {
                        match build_sync_batch(
                            &store,
                            &identity,
                            &peer_id.to_string(),
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
                        match apply_sync_batch(&store, &request) {
                            Ok((inserted, receiver_epoch, acknowledgements)) => {
                                let ack = SyncRequest::ack(
                                    identity.public_key_bytes(),
                                    receiver_epoch,
                                    acknowledgements,
                                );
                                let _ = discovery.sync_peer(peer_id, ack).await;
                                if let Some((community_id, call_id)) = active_call
                                    && is_authorized_peer(&store, community_id, peer_id)
                                {
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
                            Err(error) => format!("Lote de sincronizacao recusado: {error}"),
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
                        )).await
                    }
                    DiscoveryEvent::PeerExpired { .. } => {
                        "Procurando na rede local".to_owned()
                    }
                    DiscoveryEvent::PeerDisconnected(peer_id) => {
                        connected_peers.remove(&peer_id);
                        if let Some(engine) = call_engine.as_mut() {
                            let peer_name = peer_id.to_string();
                            let _ = Box::pin(engine.remove_peer(&peer_name)).await;
                        }
                        "Procurando na rede local".to_owned()
                    }
                };
                update_status(&app, status);
            }
        });
    });
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
    format!("{} pessoa(s) conectada(s)", engine.connected_peer_count())
}

fn update_local_video(app: &slint::Weak<AppWindow>, width: u32, height: u32, rgba: Vec<u8>) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
        buffer.make_mut_bytes().copy_from_slice(&rgba);
        let image = slint::Image::from_rgba8(buffer);
        if let Some(app) = app.upgrade() {
            app.set_local_video(image);
            app.set_has_local_video(true);
        }
    });
}

fn update_remote_video(app: &slint::Weak<AppWindow>, width: u32, height: u32, rgba: Vec<u8>) {
    let app = app.clone();
    let _ = slint::invoke_from_event_loop(move || {
        let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
        buffer.make_mut_bytes().copy_from_slice(&rgba);
        let image = slint::Image::from_rgba8(buffer);
        if let Some(app) = app.upgrade() {
            app.set_remote_video(image);
            app.set_has_remote_video(true);
        }
    });
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
                if let Some(engine) = call_engine.as_mut() {
                    let _ = Box::pin(engine.close()).await;
                }
                *call_engine = Some(engine);
                *active_call = Some((community_id, call_id));
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
                        CallSignalKind::ParticipantState,
                        "join".to_owned(),
                    )
                    .await;
                    if let Err(error) = result {
                        update_call_status(app, format!("Falha ao sinalizar entrada: {error}"));
                    }
                }
                update_call_status(app, "Aguardando outros participantes".to_owned());
            }
            Err(error) => {
                update_call_status(app, format!("Audio indisponivel: {error}"));
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
            }
        }
        CallCommand::SetScreenSharing(sharing) => {
            if let Some(engine) = call_engine.as_mut() {
                engine.set_screen_sharing(sharing);
                update_call_status(
                    app,
                    if sharing {
                        "Compartilhando tela".to_owned()
                    } else {
                        "Compartilhamento de tela encerrado".to_owned()
                    },
                );
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
                    app.set_screen_sharing(false);
                }
            });
            update_call_status(app, "Fora da voz".to_owned());
        }
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
) -> String {
    let Some((community_id, call_id)) = active_call else {
        return "Convite de voz recebido".to_owned();
    };
    let peer_name = peer_id.to_string();
    for signal in request.signals {
        if signal.author_key != request.device_key
            || signal.community_id != community_id
            || signal.call_id != call_id
        {
            continue;
        }
        let accepted = store
            .accept_call_signal(&signal, current_timestamp())
            .unwrap_or(false);
        if !accepted {
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
                CallSignalKind::Offer => {
                    if call_negotiation_role(&identity.public_key_bytes(), &signal.author_key)
                        != Some(CallNegotiationRole::Answerer)
                    {
                        anyhow::bail!("oferta recebida do lado incorreto da negociacao");
                    }
                    let answer =
                        Box::pin(engine.accept_offer(peer_name.clone(), call_id, signal.payload))
                            .await?;
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
                CallSignalKind::IceCandidate => Ok(()),
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

fn remember_listen_address(
    shared: &Arc<Mutex<Vec<String>>>,
    address: &libp2p::Multiaddr,
    peer_id: libp2p::PeerId,
) {
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
        "tailscale",
        "zerotier",
        "loopback",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

fn build_sync_batch(
    store: &LocalStore,
    identity: &DeviceIdentity,
    peer_id: &str,
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
        let (messages, has_more) = store.sync_page(
            peer_id,
            receiver_epoch,
            community_id,
            MAX_MESSAGES_PER_COMMUNITY,
            now,
        )?;
        store.record_pending(
            peer_id,
            receiver_epoch,
            community_id,
            &messages
                .iter()
                .map(|message| message.id)
                .collect::<Vec<_>>(),
        )?;
        communities.push(CommunitySync {
            community_id,
            credentials,
            messages,
            has_more,
        });
    }
    Ok(SyncRequest::batch(
        identity.public_key_bytes(),
        receiver_epoch,
        communities,
    ))
}

fn apply_sync_batch(
    store: &LocalStore,
    request: &SyncRequest,
) -> Result<(usize, uuid::Uuid, Vec<CommunityAck>)> {
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
        return Ok((0, store.database_epoch()?, Vec::new()));
    };
    if *receiver_epoch != store.database_epoch()? {
        anyhow::bail!("lote destinado a outra base local");
    }
    let now = current_timestamp();
    let mut inserted = 0;
    let mut acknowledgements = Vec::new();
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
        let (_, new_messages) =
            store.import_messages_accepted(community.community_id, &community.messages, now)?;
        inserted += new_messages;
        acknowledgements.push(CommunityAck {
            community_id: community.community_id,
            processed_message_ids: community
                .messages
                .iter()
                .map(|message| message.id)
                .collect(),
            request_next: community.has_more,
        });
    }
    Ok((inserted, *receiver_epoch, acknowledgements))
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
        if acknowledgement.request_next && acknowledged > 0 {
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
    build_sync_batch(store, identity, peer_id, *receiver_epoch, &tokens).map(Some)
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
