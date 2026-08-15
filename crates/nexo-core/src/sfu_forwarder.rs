use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MediaType {
    Audio,
    Video,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ForwardedMediaPacket {
    pub publisher_id: String,
    pub media_type: MediaType,
    pub sequence: u64,
    pub payload: Vec<u8>,
}

/// Participant-hosted SFU forwarding router.
///
/// Forwards end-to-end encrypted media packets from publishing peers to subscribed
/// receiving peers without decrypting stream contents.
#[derive(Clone, Debug, Default)]
pub struct SfuForwarder {
    subscribers: HashMap<String, HashSet<String>>,
    packets_forwarded: u64,
    bytes_forwarded: u64,
}

impl SfuForwarder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a peer as participating and subscribing to call media.
    pub fn add_peer(&mut self, peer_id: String) {
        self.subscribers.entry(peer_id).or_default();
    }

    /// Unregister a peer when they leave the call.
    pub fn remove_peer(&mut self, peer_id: &str) {
        self.subscribers.remove(peer_id);
        for subs in self.subscribers.values_mut() {
            subs.remove(peer_id);
        }
    }

    /// Subscribe `subscriber_id` to receive media from `publisher_id`.
    pub fn subscribe(&mut self, subscriber_id: &str, publisher_id: &str) -> bool {
        if !self.subscribers.contains_key(publisher_id) {
            return false;
        }
        if let Some(subs) = self.subscribers.get_mut(subscriber_id) {
            subs.insert(publisher_id.to_owned());
            true
        } else {
            false
        }
    }

    /// Routes an incoming encrypted media packet to all eligible subscribed peers.
    /// Returns a list of `(target_peer_id, packet)` pairs to be delivered.
    pub fn route_packet(
        &mut self,
        publisher_id: &str,
        media_type: MediaType,
        sequence: u64,
        payload: Vec<u8>,
    ) -> Vec<(String, ForwardedMediaPacket)> {
        if !self.subscribers.contains_key(publisher_id) {
            return Vec::new();
        }

        let packet = ForwardedMediaPacket {
            publisher_id: publisher_id.to_owned(),
            media_type,
            sequence,
            payload,
        };

        let mut destinations = Vec::new();
        for (peer_id, subs) in &self.subscribers {
            if peer_id != publisher_id && (subs.contains(publisher_id) || subs.is_empty()) {
                destinations.push((peer_id.clone(), packet.clone()));
            }
        }

        self.packets_forwarded += destinations.len() as u64;
        self.bytes_forwarded += (destinations.len() * packet.payload.len()) as u64;
        destinations
    }

    #[must_use]
    pub fn peer_count(&self) -> usize {
        self.subscribers.len()
    }

    #[must_use]
    pub fn stats(&self) -> (u64, u64) {
        (self.packets_forwarded, self.bytes_forwarded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sfu_forwarder_routes_to_subscribers_excluding_publisher() {
        let mut forwarder = SfuForwarder::new();
        forwarder.add_peer("peer_a".into());
        forwarder.add_peer("peer_b".into());
        forwarder.add_peer("peer_c".into());

        let payload = vec![1, 2, 3, 4];
        let routes = forwarder.route_packet("peer_a", MediaType::Audio, 1, payload.clone());

        assert_eq!(routes.len(), 2);
        let targets: Vec<String> = routes.into_iter().map(|(id, _)| id).collect();
        assert!(targets.contains(&"peer_b".to_string()));
        assert!(targets.contains(&"peer_c".to_string()));
        assert!(!targets.contains(&"peer_a".to_string()));

        let (pkts, bytes) = forwarder.stats();
        assert_eq!(pkts, 2);
        assert_eq!(bytes, 8);
    }

    #[test]
    fn sfu_forwarder_handles_peer_removal() {
        let mut forwarder = SfuForwarder::new();
        forwarder.add_peer("peer_a".into());
        forwarder.add_peer("peer_b".into());

        forwarder.remove_peer("peer_b");
        let routes = forwarder.route_packet("peer_a", MediaType::Audio, 1, vec![0]);
        assert!(routes.is_empty());
    }
}
