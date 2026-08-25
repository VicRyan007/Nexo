use ed25519_dalek::Signature;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{DeviceIdentity, IdentityError, NodeMetrics, SfuMigrationProposal};

const SIGNAL_VERSION: u8 = 1;
pub const MAX_SIGNAL_BYTES: usize = 32 * 1024;
const MAX_SIGNAL_AGE_SECONDS: u64 = 5 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CallSignalKind {
    Offer,
    Answer,
    IceCandidate,
    ParticipantState,
    Capabilities,
    SfuMetrics,
    SfuHeartbeat,
    SfuMigration,
    Leave,
    DirectSessionHello,
    DirectMessage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CallNegotiationRole {
    Offerer,
    Answerer,
}

#[must_use]
pub fn call_negotiation_role(
    local_key: &[u8; 32],
    remote_key: &[u8; 32],
) -> Option<CallNegotiationRole> {
    match local_key.cmp(remote_key) {
        std::cmp::Ordering::Less => Some(CallNegotiationRole::Offerer),
        std::cmp::Ordering::Greater => Some(CallNegotiationRole::Answerer),
        std::cmp::Ordering::Equal => None,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CallSignal {
    pub version: u8,
    pub id: Uuid,
    pub community_id: Uuid,
    pub call_id: Uuid,
    pub sequence: u64,
    pub kind: CallSignalKind,
    pub payload: String,
    pub author_key: [u8; 32],
    pub created_at: u64,
    pub signature: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum CallSignalError {
    #[error("the call signal version is unsupported")]
    UnsupportedVersion,
    #[error("the call signal payload exceeds {MAX_SIGNAL_BYTES} bytes")]
    PayloadTooLarge,
    #[error("the call signal is empty")]
    EmptyPayload,
    #[error("the call signal timestamp is too far in the future")]
    FutureTimestamp,
    #[error("the call signal has expired")]
    StaleTimestamp,
    #[error("the participant state payload is invalid")]
    InvalidParticipantState,
    #[error("the call capabilities payload is invalid")]
    InvalidCapabilities,
    #[error("the SFU metrics payload is invalid")]
    InvalidSfuMetrics,
    #[error("the SFU heartbeat payload is invalid")]
    InvalidSfuHeartbeat,
    #[error("the SFU migration payload is invalid")]
    InvalidSfuMigration,
    #[error("the call signal signature has an invalid length")]
    InvalidSignatureLength,
    #[error("the call signal could not be serialized: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("the call signal signature is invalid: {0}")]
    Identity(#[from] IdentityError),
}

#[derive(Serialize)]
struct SignedFields<'a> {
    version: u8,
    id: Uuid,
    community_id: Uuid,
    call_id: Uuid,
    sequence: u64,
    kind: CallSignalKind,
    payload: &'a str,
    author_key: &'a [u8; 32],
    created_at: u64,
}

impl CallSignal {
    pub fn create(
        identity: &DeviceIdentity,
        community_id: Uuid,
        call_id: Uuid,
        sequence: u64,
        kind: CallSignalKind,
        payload: String,
        created_at: u64,
    ) -> Result<Self, CallSignalError> {
        validate_payload(kind, &payload)?;
        let mut signal = Self {
            version: SIGNAL_VERSION,
            id: Uuid::new_v4(),
            community_id,
            call_id,
            sequence,
            kind,
            payload,
            author_key: identity.public_key_bytes(),
            created_at,
            signature: Vec::new(),
        };
        signal.signature = identity.sign(&signal.signing_bytes()?).to_vec();
        Ok(signal)
    }

    pub fn verify(&self, now: u64) -> Result<(), CallSignalError> {
        if self.version != SIGNAL_VERSION {
            return Err(CallSignalError::UnsupportedVersion);
        }
        validate_payload(self.kind, &self.payload)?;
        if self.created_at > now.saturating_add(5 * 60) {
            return Err(CallSignalError::FutureTimestamp);
        }
        if now.saturating_sub(self.created_at) > MAX_SIGNAL_AGE_SECONDS {
            return Err(CallSignalError::StaleTimestamp);
        }
        let signature: [u8; Signature::BYTE_SIZE] = self
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| CallSignalError::InvalidSignatureLength)?;
        DeviceIdentity::verify(&self.author_key, &self.signing_bytes()?, &signature)?;
        Ok(())
    }

    fn signing_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&SignedFields {
            version: self.version,
            id: self.id,
            community_id: self.community_id,
            call_id: self.call_id,
            sequence: self.sequence,
            kind: self.kind,
            payload: &self.payload,
            author_key: &self.author_key,
            created_at: self.created_at,
        })
    }
}

fn validate_payload(kind: CallSignalKind, payload: &str) -> Result<(), CallSignalError> {
    if payload.len() > MAX_SIGNAL_BYTES {
        return Err(CallSignalError::PayloadTooLarge);
    }
    if payload.is_empty() && kind != CallSignalKind::Leave {
        return Err(CallSignalError::EmptyPayload);
    }
    if kind == CallSignalKind::ParticipantState && !matches!(payload, "join" | "present") {
        return Err(CallSignalError::InvalidParticipantState);
    }
    if kind == CallSignalKind::Capabilities && !matches!(payload, "video=vp8" | "video=vp8,h264") {
        return Err(CallSignalError::InvalidCapabilities);
    }
    if kind == CallSignalKind::SfuMetrics
        && NodeMetrics::from_signal_payload("signed-peer", payload).is_none()
    {
        return Err(CallSignalError::InvalidSfuMetrics);
    }
    if kind == CallSignalKind::SfuHeartbeat && payload != "heartbeat" {
        return Err(CallSignalError::InvalidSfuHeartbeat);
    }
    if kind == CallSignalKind::SfuMigration
        && SfuMigrationProposal::from_signal_payload(payload).is_none()
    {
        return Err(CallSignalError::InvalidSfuMigration);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_is_bound_to_call_and_payload() {
        let identity = DeviceIdentity::generate();
        let mut signal = CallSignal::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            1,
            CallSignalKind::Offer,
            "v=0".into(),
            100,
        )
        .expect("signal should be created");
        signal.verify(100).expect("signal should verify");
        signal.payload.push_str("tampered");
        assert!(signal.verify(100).is_err());
    }

    #[test]
    fn ephemeral_call_signal_expires() {
        let identity = DeviceIdentity::generate();
        let signal = CallSignal::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            1,
            CallSignalKind::ParticipantState,
            "join".into(),
            100,
        )
        .expect("signal should be created");
        assert!(matches!(
            signal.verify(100 + MAX_SIGNAL_AGE_SECONDS + 1),
            Err(CallSignalError::StaleTimestamp)
        ));
    }

    #[test]
    fn unknown_participant_state_is_rejected() {
        let identity = DeviceIdentity::generate();
        assert!(matches!(
            CallSignal::create(
                &identity,
                Uuid::new_v4(),
                Uuid::new_v4(),
                1,
                CallSignalKind::ParticipantState,
                "unexpected".into(),
                100,
            ),
            Err(CallSignalError::InvalidParticipantState)
        ));
    }

    #[test]
    fn capabilities_are_bounded_to_known_video_codecs() {
        let identity = DeviceIdentity::generate();
        let signal = CallSignal::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            1,
            CallSignalKind::Capabilities,
            "video=vp8,h264".into(),
            100,
        )
        .expect("known capabilities should be accepted");
        signal.verify(100).expect("capabilities should verify");
        assert!(matches!(
            CallSignal::create(
                &identity,
                Uuid::new_v4(),
                Uuid::new_v4(),
                2,
                CallSignalKind::Capabilities,
                "video=av1".into(),
                100,
            ),
            Err(CallSignalError::InvalidCapabilities)
        ));
    }

    #[test]
    fn sfu_control_signals_reject_unbounded_payloads() {
        let identity = DeviceIdentity::generate();
        let metrics = "up=1000;loss=5;rtt=10;cpu=600;gpu=700;enc=1;reach=0";
        let signal = CallSignal::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            1,
            CallSignalKind::SfuMetrics,
            metrics.into(),
            100,
        )
        .expect("valid SFU metrics should be accepted");
        signal.verify(100).expect("metrics should verify");
        let migration = CallSignal::create(
            &identity,
            Uuid::new_v4(),
            Uuid::new_v4(),
            2,
            CallSignalKind::SfuMigration,
            "term=2;from=node1;to=node2".into(),
            100,
        )
        .expect("valid SFU migration should be accepted");
        migration.verify(100).expect("migration should verify");
        assert!(matches!(
            CallSignal::create(
                &identity,
                Uuid::new_v4(),
                Uuid::new_v4(),
                2,
                CallSignalKind::SfuHeartbeat,
                "wrong".into(),
                100,
            ),
            Err(CallSignalError::InvalidSfuHeartbeat)
        ));
        assert!(matches!(
            CallSignal::create(
                &identity,
                Uuid::new_v4(),
                Uuid::new_v4(),
                3,
                CallSignalKind::SfuMigration,
                "term=2;from=node1;to=node1".into(),
                100,
            ),
            Err(CallSignalError::InvalidSfuMigration)
        ));
    }

    #[test]
    fn negotiation_roles_are_complementary_and_reject_self() {
        let first = DeviceIdentity::generate().public_key_bytes();
        let second = DeviceIdentity::generate().public_key_bytes();
        let first_role = call_negotiation_role(&first, &second);
        let second_role = call_negotiation_role(&second, &first);
        assert!(matches!(
            (first_role, second_role),
            (
                Some(CallNegotiationRole::Offerer),
                Some(CallNegotiationRole::Answerer)
            ) | (
                Some(CallNegotiationRole::Answerer),
                Some(CallNegotiationRole::Offerer)
            )
        ));
        assert_eq!(call_negotiation_role(&first, &first), None);
    }
}
