use nexo_core::CallSignal;
use serde::{Deserialize, Serialize};

pub const SIGNAL_PROTOCOL: &str = "/nexo/call-signal/0.1.0";
pub const MAX_SIGNALS_PER_REQUEST: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignalRequest {
    pub version: u8,
    pub device_key: [u8; 32],
    pub signals: Vec<CallSignal>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SignalResponse {
    pub version: u8,
    pub received: u16,
}

impl SignalRequest {
    #[must_use]
    pub fn new(device_key: [u8; 32], mut signals: Vec<CallSignal>) -> Self {
        signals.truncate(MAX_SIGNALS_PER_REQUEST);
        Self {
            version: 1,
            device_key,
            signals,
        }
    }

    #[must_use]
    pub fn is_within_limits(&self) -> bool {
        self.version == 1 && self.signals.len() <= MAX_SIGNALS_PER_REQUEST
    }
}

impl SignalResponse {
    #[must_use]
    pub fn received(count: usize) -> Self {
        Self {
            version: 1,
            received: u16::try_from(count.min(MAX_SIGNALS_PER_REQUEST)).unwrap_or_default(),
        }
    }
}
