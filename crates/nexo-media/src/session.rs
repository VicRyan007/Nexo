use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CallState {
    Idle,
    Joining,
    Connected,
    Reconnecting,
    Leaving,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct ParticipantState {
    pub device_id: String,
    pub muted: bool,
    pub deafened: bool,
    pub camera_enabled: bool,
    pub screen_enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallCommand {
    Join { room_id: Uuid },
    Connected,
    SetMuted(bool),
    SetDeafened(bool),
    SetCamera(bool),
    SetScreen(bool),
    PeerJoined(ParticipantState),
    PeerLeft(String),
    TransportLost,
    Leave,
    Left,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CallEvent {
    StartTransport { room_id: Uuid },
    StopTransport,
    PublishLocalState(ParticipantState),
    ReconnectTransport,
    StateChanged(CallState),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum MediaError {
    #[error("the command is not valid in the current call state")]
    InvalidTransition,
    #[error("the audio device operation failed: {0}")]
    AudioDevice(String),
}

pub struct CallSession {
    state: CallState,
    local: ParticipantState,
    peers: BTreeMap<String, ParticipantState>,
}

impl CallSession {
    #[must_use]
    pub fn new(device_id: String) -> Self {
        Self {
            state: CallState::Idle,
            local: ParticipantState {
                device_id,
                muted: false,
                deafened: false,
                camera_enabled: false,
                screen_enabled: false,
            },
            peers: BTreeMap::new(),
        }
    }

    #[must_use]
    pub const fn state(&self) -> CallState {
        self.state
    }

    pub fn participants(&self) -> impl Iterator<Item = &ParticipantState> {
        self.peers.values()
    }

    pub fn apply(&mut self, command: CallCommand) -> Result<Vec<CallEvent>, MediaError> {
        match command {
            CallCommand::Join { room_id } if self.state == CallState::Idle => {
                self.state = CallState::Joining;
                Ok(vec![
                    CallEvent::StateChanged(self.state),
                    CallEvent::StartTransport { room_id },
                ])
            }
            CallCommand::Connected
                if matches!(self.state, CallState::Joining | CallState::Reconnecting) =>
            {
                self.state = CallState::Connected;
                Ok(vec![CallEvent::StateChanged(self.state)])
            }
            CallCommand::SetMuted(value) if self.state == CallState::Connected => {
                self.local.muted = value;
                Ok(vec![CallEvent::PublishLocalState(self.local.clone())])
            }
            CallCommand::SetDeafened(value) if self.state == CallState::Connected => {
                self.local.deafened = value;
                if value {
                    self.local.muted = true;
                }
                Ok(vec![CallEvent::PublishLocalState(self.local.clone())])
            }
            CallCommand::SetCamera(value) if self.state == CallState::Connected => {
                self.local.camera_enabled = value;
                Ok(vec![CallEvent::PublishLocalState(self.local.clone())])
            }
            CallCommand::SetScreen(value) if self.state == CallState::Connected => {
                self.local.screen_enabled = value;
                Ok(vec![CallEvent::PublishLocalState(self.local.clone())])
            }
            CallCommand::PeerJoined(peer) if self.state == CallState::Connected => {
                self.peers.insert(peer.device_id.clone(), peer);
                Ok(Vec::new())
            }
            CallCommand::PeerLeft(device_id) if self.state != CallState::Idle => {
                self.peers.remove(&device_id);
                Ok(Vec::new())
            }
            CallCommand::TransportLost if self.state == CallState::Connected => {
                self.state = CallState::Reconnecting;
                Ok(vec![
                    CallEvent::StateChanged(self.state),
                    CallEvent::ReconnectTransport,
                ])
            }
            CallCommand::Leave if self.state != CallState::Idle => {
                self.state = CallState::Leaving;
                Ok(vec![
                    CallEvent::StateChanged(self.state),
                    CallEvent::StopTransport,
                ])
            }
            CallCommand::Left if self.state == CallState::Leaving => {
                self.state = CallState::Idle;
                self.peers.clear();
                Ok(vec![CallEvent::StateChanged(self.state)])
            }
            _ => Err(MediaError::InvalidTransition),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_reconnects_and_returns_to_idle_cleanly() {
        let mut call = CallSession::new("alice".into());
        call.apply(CallCommand::Join {
            room_id: Uuid::new_v4(),
        })
        .expect("join should start");
        call.apply(CallCommand::Connected)
            .expect("call should connect");
        call.apply(CallCommand::TransportLost)
            .expect("connected call should reconnect");
        call.apply(CallCommand::Connected)
            .expect("call should recover");
        call.apply(CallCommand::Leave).expect("leave should start");
        call.apply(CallCommand::Left).expect("leave should finish");
        assert_eq!(call.state(), CallState::Idle);
        assert_eq!(call.participants().count(), 0);
    }

    #[test]
    fn deafening_also_mutes_the_local_microphone() {
        let mut call = CallSession::new("alice".into());
        call.apply(CallCommand::Join {
            room_id: Uuid::new_v4(),
        })
        .expect("join should start");
        call.apply(CallCommand::Connected)
            .expect("call should connect");
        let events = call
            .apply(CallCommand::SetDeafened(true))
            .expect("deafen should apply");
        let CallEvent::PublishLocalState(local) = &events[0] else {
            panic!("local state should be published");
        };
        assert!(local.deafened);
        assert!(local.muted);
    }
}
