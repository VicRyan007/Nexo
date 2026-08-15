use serde::{Deserialize, Serialize};

/// An external ICE server (STUN or TURN) configuration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IceServer {
    pub urls: Vec<String>,
    pub username: Option<String>,
    pub credential: Option<String>,
}

impl IceServer {
    #[must_use]
    pub fn stun(url: impl Into<String>) -> Self {
        Self {
            urls: vec![url.into()],
            username: None,
            credential: None,
        }
    }

    #[must_use]
    pub fn turn(
        url: impl Into<String>,
        username: impl Into<String>,
        credential: impl Into<String>,
    ) -> Self {
        Self {
            urls: vec![url.into()],
            username: Some(username.into()),
            credential: Some(credential.into()),
        }
    }
}

/// Optional NAT Traversal configuration for cross-network (WAN/Internet) connections.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NatConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<IceServer>,
    pub prefer_direct_lan: bool,
}

impl Default for NatConfig {
    fn default() -> Self {
        Self {
            stun_servers: Vec::new(),
            turn_servers: Vec::new(),
            prefer_direct_lan: true,
        }
    }
}

impl NatConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_stun(&mut self, url: impl Into<String>) {
        self.stun_servers.push(url.into());
    }

    pub fn add_turn(&mut self, server: IceServer) {
        self.turn_servers.push(server);
    }

    #[must_use]
    pub fn has_servers(&self) -> bool {
        !self.stun_servers.is_empty() || !self.turn_servers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat_config_serializes_and_manages_servers() {
        let mut config = NatConfig::new();
        assert!(!config.has_servers());

        config.add_stun("stun:stun.l.google.com:19302");
        config.add_turn(IceServer::turn(
            "turn:nexo.example.com:3478",
            "user",
            "pass",
        ));

        assert!(config.has_servers());
        assert_eq!(config.stun_servers.len(), 1);
        assert_eq!(config.turn_servers.len(), 1);

        let json = serde_json::to_string(&config).expect("serialization works");
        let deserialized: NatConfig = serde_json::from_str(&json).expect("deserialization works");
        assert_eq!(config, deserialized);
    }
}
