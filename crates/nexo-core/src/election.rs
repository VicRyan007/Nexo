use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq)]
pub struct NodeMetrics {
    pub node_id: String,
    pub available_upload_mbps: f32,
    pub packet_loss_percent: f32,
    pub round_trip_ms: f32,
    pub cpu_headroom_percent: f32,
    pub gpu_headroom_percent: f32,
    pub hardware_encoder: bool,
    pub publicly_reachable: bool,
}

impl NodeMetrics {
    /// Calculate measured capacity score from node metrics (0.0 to 100.0).
    #[must_use]
    pub fn calculate_capacity_score(&self) -> f32 {
        score(self)
    }

    /// Compact, bounded representation exchanged through authenticated call
    /// signals. Values use tenths to avoid locale-dependent float formatting.
    #[must_use]
    pub fn signal_payload(&self) -> String {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        fn tenths(value: f32) -> u32 {
            if value.is_finite() {
                (value.clamp(0.0, 10_000.0) * 10.0).round() as u32
            } else {
                0
            }
        }

        format!(
            "up={};loss={};rtt={};cpu={};gpu={};enc={};reach={}",
            tenths(self.available_upload_mbps),
            tenths(self.packet_loss_percent),
            tenths(self.round_trip_ms),
            tenths(self.cpu_headroom_percent),
            tenths(self.gpu_headroom_percent),
            u8::from(self.hardware_encoder),
            u8::from(self.publicly_reachable),
        )
    }

    /// Parse the compact metrics payload and bind it to the authenticated node
    /// that carried the signal. Unknown, duplicated, missing, or out-of-range
    /// fields are rejected before they can influence election.
    #[allow(clippy::cast_precision_loss)]
    #[must_use]
    pub fn from_signal_payload(node_id: &str, payload: &str) -> Option<Self> {
        let mut values = HashMap::new();
        for item in payload.split(';') {
            let (key, value) = item.split_once('=')?;
            if values.insert(key, value).is_some() {
                return None;
            }
        }
        if values.len() != 7
            || !values.keys().all(|key| {
                matches!(
                    *key,
                    "up" | "loss" | "rtt" | "cpu" | "gpu" | "enc" | "reach"
                )
            })
        {
            return None;
        }
        let parse_tenths = |key: &str, max: f32| {
            let value = values.get(key)?.parse::<u32>().ok()?;
            let value = value as f32 / 10.0;
            (value <= max).then_some(value)
        };
        let hardware_encoder = match *values.get("enc")? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        let publicly_reachable = match *values.get("reach")? {
            "0" => false,
            "1" => true,
            _ => return None,
        };
        Some(Self {
            node_id: node_id.to_owned(),
            available_upload_mbps: parse_tenths("up", 10_000.0)?,
            packet_loss_percent: parse_tenths("loss", 100.0)?,
            round_trip_ms: parse_tenths("rtt", 60_000.0)?,
            cpu_headroom_percent: parse_tenths("cpu", 100.0)?,
            gpu_headroom_percent: parse_tenths("gpu", 100.0)?,
            hardware_encoder,
            publicly_reachable,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ElectionPolicy {
    pub minimum_upload_mbps: f32,
    pub maximum_packet_loss_percent: f32,
    pub switch_improvement_percent: f32,
    pub heartbeat_timeout_seconds: u64,
}

impl Default for ElectionPolicy {
    fn default() -> Self {
        Self {
            minimum_upload_mbps: 20.0,
            maximum_packet_loss_percent: 8.0,
            switch_improvement_percent: 25.0,
            heartbeat_timeout_seconds: 5,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SfuMigrationState {
    Stable,
    Migrating {
        current_host: String,
        target_host: String,
        started_at: u64,
    },
}

/// Authenticated call-signal payload proposed by the active relay when a
/// materially better participant should take over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SfuMigrationProposal {
    pub term: u64,
    pub from: String,
    pub to: String,
}

impl SfuMigrationProposal {
    const MAX_PEER_ID_BYTES: usize = 128;

    #[must_use]
    pub fn new(term: u64, from: String, to: String) -> Option<Self> {
        let proposal = Self { term, from, to };
        proposal.is_valid().then_some(proposal)
    }

    #[must_use]
    pub fn to_signal_payload(&self) -> String {
        format!("term={};from={};to={}", self.term, self.from, self.to)
    }

    #[must_use]
    pub fn from_signal_payload(payload: &str) -> Option<Self> {
        let mut term = None;
        let mut from = None;
        let mut to = None;
        for item in payload.split(';') {
            let (key, value) = item.split_once('=')?;
            match key {
                "term" if term.is_none() => term = value.parse::<u64>().ok(),
                "from" if from.is_none() => from = Some(value.to_owned()),
                "to" if to.is_none() => to = Some(value.to_owned()),
                _ => return None,
            }
        }
        Self::new(term?, from?, to?)
    }

    fn is_valid(&self) -> bool {
        self.term > 0
            && valid_peer_id(&self.from)
            && valid_peer_id(&self.to)
            && self.from != self.to
    }
}

fn valid_peer_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= SfuMigrationProposal::MAX_PEER_ID_BYTES
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SfuTopologyEvent {
    HostElected { host_id: String },
    StandbyElected { standby_id: String },
    MigrationStarted { from: String, to: String },
    MigrationCompleted { new_host: String },
    HostTimedOut { failed_host: String },
}

/// Participant-hosted SFU topology manager.
///
/// Tracks the active SFU host, standby host, node heartbeats, and drives
/// make-before-break migrations and failure recovery.
#[derive(Clone, Debug, PartialEq)]
pub struct SfuTopology {
    active_host: Option<String>,
    standby_host: Option<String>,
    migration_state: SfuMigrationState,
    last_heartbeat: HashMap<String, u64>,
    policy: ElectionPolicy,
    deterministic_initial: bool,
    current_term: u64,
}

impl SfuTopology {
    #[must_use]
    pub fn new(policy: ElectionPolicy) -> Self {
        Self::with_initial_mode(policy, false)
    }

    /// Builds a topology whose first host and standby are selected by stable
    /// peer identity. This prevents independent call participants from
    /// electing different hosts while their local metric samples are still
    /// settling; subsequent migrations still use capacity and hysteresis.
    #[must_use]
    pub fn new_convergent(policy: ElectionPolicy) -> Self {
        Self::with_initial_mode(policy, true)
    }

    fn with_initial_mode(policy: ElectionPolicy, deterministic_initial: bool) -> Self {
        Self {
            active_host: None,
            standby_host: None,
            migration_state: SfuMigrationState::Stable,
            last_heartbeat: HashMap::new(),
            policy,
            deterministic_initial,
            current_term: 0,
        }
    }

    #[must_use]
    pub fn active_host(&self) -> Option<&str> {
        self.active_host.as_deref()
    }

    #[must_use]
    pub fn standby_host(&self) -> Option<&str> {
        self.standby_host.as_deref()
    }

    /// Establishes the deterministic bootstrap host before all metric
    /// signals have arrived. Capacity-based migration remains available once
    /// the complete call snapshot is assembled.
    pub fn establish_initial_host(
        &mut self,
        host_id: &str,
        standby_id: Option<&str>,
    ) -> Vec<SfuTopologyEvent> {
        if !self.deterministic_initial || self.active_host.is_some() || host_id.is_empty() {
            return Vec::new();
        }

        self.active_host = Some(host_id.to_owned());
        self.current_term = self.current_term.max(1);
        let mut events = vec![SfuTopologyEvent::HostElected {
            host_id: host_id.to_owned(),
        }];
        if let Some(standby) = standby_id.filter(|standby| *standby != host_id) {
            let standby = standby.to_owned();
            self.standby_host = Some(standby.clone());
            events.push(SfuTopologyEvent::StandbyElected {
                standby_id: standby,
            });
        }
        events
    }

    #[must_use]
    pub fn migration_state(&self) -> &SfuMigrationState {
        &self.migration_state
    }

    #[must_use]
    pub fn term(&self) -> u64 {
        self.current_term
    }

    pub fn record_heartbeat(&mut self, node_id: &str, timestamp: u64) {
        self.last_heartbeat.insert(node_id.to_owned(), timestamp);
    }

    /// Remove a peer immediately when the authenticated transport reports it
    /// disconnected. The next `update` can then migrate away from an active
    /// host without waiting for the heartbeat timeout; a removed migration
    /// target is cancelled so the topology cannot remain stuck forever.
    pub fn remove_node(&mut self, node_id: &str) {
        self.last_heartbeat.remove(node_id);
        if self.standby_host.as_deref() == Some(node_id) {
            self.standby_host = None;
        }
        if matches!(
            &self.migration_state,
            SfuMigrationState::Migrating { target_host, .. } if target_host == node_id
        ) {
            self.migration_state = SfuMigrationState::Stable;
        }
    }

    /// Evaluates current metrics and updates the host and standby selections.
    /// Returns events triggered by election or migration changes.
    pub fn update(&mut self, nodes: &[NodeMetrics], now: u64) -> Vec<SfuTopologyEvent> {
        self.update_with_role(nodes, now, true)
    }

    /// Evaluates topology while optionally allowing the local active relay to
    /// initiate a capacity migration. Failure recovery remains enabled for
    /// replicas when this flag is false.
    #[allow(clippy::too_many_lines)]
    pub fn update_with_role(
        &mut self,
        nodes: &[NodeMetrics],
        now: u64,
        allow_capacity_migration: bool,
    ) -> Vec<SfuTopologyEvent> {
        let mut events = Vec::new();

        // Filter out nodes that have timed out on heartbeat if we have a record
        let active_nodes: Vec<NodeMetrics> = nodes
            .iter()
            .filter(|node| {
                self.last_heartbeat.get(&node.node_id).is_none_or(|last| {
                    now.saturating_sub(*last) <= self.policy.heartbeat_timeout_seconds
                })
            })
            .cloned()
            .collect();

        let mut eligible: Vec<_> = active_nodes
            .iter()
            .filter(|node| is_eligible(node, self.policy))
            .collect();
        eligible.sort_by(|left, right| {
            score(right)
                .total_cmp(&score(left))
                .then_with(|| left.node_id.cmp(&right.node_id))
        });

        let top_candidate = eligible.first().copied();
        // The first election must converge even when each participant's
        // local CPU sample was taken at a slightly different instant. Use a
        // stable identity tie-break for the initial host and standby; later
        // updates can still migrate to a materially better candidate.
        let initial_host = eligible
            .iter()
            .min_by(|left, right| left.node_id.cmp(&right.node_id))
            .copied();
        let initial_standby = eligible
            .iter()
            .filter(|node| initial_host.is_none_or(|host| node.node_id != host.node_id))
            .min_by(|left, right| left.node_id.cmp(&right.node_id))
            .copied();

        match (self.active_host.as_deref(), top_candidate) {
            (None, Some(candidate)) => {
                let candidate = if self.deterministic_initial {
                    initial_host
                } else {
                    Some(candidate)
                };
                let Some(candidate) = candidate else {
                    return events;
                };
                let host_id = candidate.node_id.clone();
                self.active_host = Some(host_id.clone());
                self.current_term = self.current_term.max(1);
                events.push(SfuTopologyEvent::HostElected {
                    host_id: host_id.clone(),
                });

                let standby = if self.deterministic_initial {
                    initial_standby
                } else {
                    eligible.get(1).copied()
                };
                if let Some(standby) = standby {
                    let standby_id = standby.node_id.clone();
                    self.standby_host = Some(standby_id.clone());
                    events.push(SfuTopologyEvent::StandbyElected { standby_id });
                } else {
                    self.standby_host = None;
                }
            }
            (Some(incumbent_id), Some(candidate)) => {
                let incumbent = active_nodes
                    .iter()
                    .find(|n| n.node_id == incumbent_id && is_eligible(n, self.policy));
                let candidate = if self.deterministic_initial && incumbent.is_none() {
                    initial_host.unwrap_or(candidate)
                } else {
                    candidate
                };

                let should_switch = if let Some(incumbent_node) = incumbent {
                    if !allow_capacity_migration || candidate.node_id == incumbent_node.node_id {
                        false
                    } else {
                        let required = score(incumbent_node)
                            * (1.0 + self.policy.switch_improvement_percent / 100.0);
                        score(candidate) >= required
                    }
                } else {
                    // Incumbent is no longer eligible or active
                    true
                };

                if should_switch && self.migration_state == SfuMigrationState::Stable {
                    let target_id = candidate.node_id.clone();
                    self.current_term = self.current_term.saturating_add(1).max(1);
                    self.migration_state = SfuMigrationState::Migrating {
                        current_host: incumbent_id.to_owned(),
                        target_host: target_id.clone(),
                        started_at: now,
                    };
                    events.push(SfuTopologyEvent::MigrationStarted {
                        from: incumbent_id.to_owned(),
                        to: target_id,
                    });
                }

                // Update standby host (best candidate that is not the active host)
                let standby_candidate = if self.deterministic_initial {
                    eligible
                        .iter()
                        .filter(|node| node.node_id != incumbent_id)
                        .min_by(|left, right| left.node_id.cmp(&right.node_id))
                        .copied()
                } else {
                    eligible.iter().find(|n| n.node_id != incumbent_id).copied()
                };
                if let Some(standby) = standby_candidate {
                    if self.standby_host.as_deref() != Some(&standby.node_id) {
                        self.standby_host = Some(standby.node_id.clone());
                        events.push(SfuTopologyEvent::StandbyElected {
                            standby_id: standby.node_id.clone(),
                        });
                    }
                } else {
                    self.standby_host = None;
                }
            }
            (Some(incumbent_id), None) => {
                // No eligible candidates remain
                events.push(SfuTopologyEvent::HostTimedOut {
                    failed_host: incumbent_id.to_owned(),
                });
                self.active_host = None;
                self.standby_host = None;
                self.migration_state = SfuMigrationState::Stable;
            }
            (None, None) => {}
        }

        events
    }

    /// Accept a newer migration proposal authenticated by the current host.
    pub fn accept_migration(&mut self, proposal: &SfuMigrationProposal, now: u64) -> bool {
        if proposal.term <= self.current_term
            || self.active_host.as_deref() != Some(proposal.from.as_str())
            || self.migration_state != SfuMigrationState::Stable
        {
            return false;
        }
        self.current_term = proposal.term;
        self.migration_state = SfuMigrationState::Migrating {
            current_host: proposal.from.clone(),
            target_host: proposal.to.clone(),
            started_at: now,
        };
        true
    }

    /// Confirm completion of make-before-break migration to the target host.
    pub fn confirm_migration(&mut self) -> Option<SfuTopologyEvent> {
        if let SfuMigrationState::Migrating {
            ref target_host, ..
        } = self.migration_state
        {
            let new_host = target_host.clone();
            self.active_host = Some(new_host.clone());
            self.migration_state = SfuMigrationState::Stable;
            Some(SfuTopologyEvent::MigrationCompleted { new_host })
        } else {
            None
        }
    }

    /// Checks heartbeats and handles host timeouts by failing over to standby.
    pub fn check_heartbeat_timeout(&mut self, now: u64) -> Vec<SfuTopologyEvent> {
        let mut events = Vec::new();
        if let Some(ref host_id) = self.active_host {
            let timed_out = self.last_heartbeat.get(host_id).is_some_and(|last| {
                now.saturating_sub(*last) > self.policy.heartbeat_timeout_seconds
            });

            if timed_out {
                let failed_host = host_id.clone();
                events.push(SfuTopologyEvent::HostTimedOut {
                    failed_host: failed_host.clone(),
                });

                let standby_is_alive = self.standby_host.as_deref().is_some_and(|standby_id| {
                    self.last_heartbeat.get(standby_id).is_some_and(|last| {
                        now.saturating_sub(*last) <= self.policy.heartbeat_timeout_seconds
                    })
                });
                if standby_is_alive {
                    if let Some(standby_id) = self.standby_host.take() {
                        self.current_term = self.current_term.saturating_add(1).max(1);
                        self.active_host = Some(standby_id.clone());
                        events.push(SfuTopologyEvent::HostElected {
                            host_id: standby_id,
                        });
                    } else {
                        self.active_host = None;
                    }
                } else {
                    self.standby_host = None;
                    self.active_host = None;
                }
                self.migration_state = SfuMigrationState::Stable;
            }
        }
        events
    }
}

#[must_use]
pub fn elect_host<'a>(
    nodes: &'a [NodeMetrics],
    incumbent_id: Option<&str>,
    policy: ElectionPolicy,
) -> Option<&'a NodeMetrics> {
    let mut eligible: Vec<_> = nodes
        .iter()
        .filter(|node| is_eligible(node, policy))
        .collect();
    eligible.sort_by(|left, right| {
        score(right)
            .total_cmp(&score(left))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let candidate = eligible.first().copied()?;

    let Some(incumbent) = incumbent_id.and_then(|id| {
        nodes
            .iter()
            .find(|node| node.node_id == id && is_eligible(node, policy))
    }) else {
        return Some(candidate);
    };

    if candidate.node_id == incumbent.node_id {
        return Some(incumbent);
    }
    let required = score(incumbent) * (1.0 + policy.switch_improvement_percent / 100.0);
    (score(candidate) >= required)
        .then_some(candidate)
        .or(Some(incumbent))
}

fn is_eligible(node: &NodeMetrics, policy: ElectionPolicy) -> bool {
    node.available_upload_mbps.is_finite()
        && node.available_upload_mbps >= policy.minimum_upload_mbps
        && node.packet_loss_percent.is_finite()
        && (0.0..=policy.maximum_packet_loss_percent).contains(&node.packet_loss_percent)
        && node.round_trip_ms.is_finite()
        && node.round_trip_ms >= 0.0
}

fn score(node: &NodeMetrics) -> f32 {
    let upload = (node.available_upload_mbps.min(200.0) / 200.0) * 40.0;
    let stability = (1.0 - node.packet_loss_percent.clamp(0.0, 10.0) / 10.0) * 20.0;
    let latency = (1.0 - node.round_trip_ms.clamp(0.0, 300.0) / 300.0) * 15.0;
    let compute = (node.cpu_headroom_percent.clamp(0.0, 100.0) / 100.0) * 10.0
        + (node.gpu_headroom_percent.clamp(0.0, 100.0) / 100.0) * 5.0;
    let encoder = if node.hardware_encoder { 5.0 } else { 0.0 };
    let reachability = if node.publicly_reachable { 5.0 } else { 0.0 };
    upload + stability + latency + compute + encoder + reachability
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, upload: f32) -> NodeMetrics {
        NodeMetrics {
            node_id: id.into(),
            available_upload_mbps: upload,
            packet_loss_percent: 0.5,
            round_trip_ms: 20.0,
            cpu_headroom_percent: 70.0,
            gpu_headroom_percent: 70.0,
            hardware_encoder: true,
            publicly_reachable: true,
        }
    }

    #[test]
    fn elects_best_eligible_node() {
        let nodes = [node("slow", 25.0), node("fast", 100.0)];
        assert_eq!(
            elect_host(&nodes, None, ElectionPolicy::default())
                .expect("an eligible node should be elected")
                .node_id,
            "fast"
        );
    }

    #[test]
    fn node_metrics_round_trip_through_bounded_signal_payload() {
        let original = node("peer", 100.0);
        let payload = original.signal_payload();
        let decoded = NodeMetrics::from_signal_payload("peer", &payload)
            .expect("metrics payload should parse");
        assert_eq!(decoded.node_id, "peer");
        assert!(
            (decoded.available_upload_mbps - original.available_upload_mbps).abs() < f32::EPSILON
        );
        assert_eq!(decoded.hardware_encoder, original.hardware_encoder);
        assert!(NodeMetrics::from_signal_payload("peer", "up=1;up=2").is_none());
    }

    #[test]
    fn migration_proposal_round_trips_and_rejects_stale_or_malformed_ids() {
        let proposal = SfuMigrationProposal::new(4, "node1".into(), "node2".into())
            .expect("valid proposal should be created");
        assert_eq!(
            SfuMigrationProposal::from_signal_payload(&proposal.to_signal_payload()),
            Some(proposal)
        );
        assert!(SfuMigrationProposal::from_signal_payload("term=0;from=node1;to=node2").is_none());
        assert!(SfuMigrationProposal::from_signal_payload("term=4;from=node-1;to=node2").is_none());
    }

    #[test]
    fn hysteresis_keeps_incumbent_for_small_improvement() {
        let nodes = [node("current", 90.0), node("candidate", 100.0)];
        assert_eq!(
            elect_host(&nodes, Some("current"), ElectionPolicy::default())
                .expect("the incumbent should remain eligible")
                .node_id,
            "current"
        );
    }

    #[test]
    fn sfu_topology_elects_host_and_standby() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        let nodes = [
            node("node1", 50.0),
            node("node2", 100.0),
            node("node3", 30.0),
        ];

        let events = topology.update(&nodes, 100);
        assert_eq!(topology.active_host(), Some("node2"));
        assert_eq!(topology.standby_host(), Some("node1"));

        assert!(events.contains(&SfuTopologyEvent::HostElected {
            host_id: "node2".into()
        }));
        assert!(events.contains(&SfuTopologyEvent::StandbyElected {
            standby_id: "node1".into()
        }));
    }

    #[test]
    fn deterministic_bootstrap_host_does_not_wait_for_metrics() {
        let mut topology = SfuTopology::new_convergent(ElectionPolicy::default());
        let events = topology.establish_initial_host("node-a", Some("node-b"));
        assert_eq!(topology.active_host(), Some("node-a"));
        assert_eq!(topology.standby_host(), Some("node-b"));
        assert!(events.contains(&SfuTopologyEvent::HostElected {
            host_id: "node-a".to_owned()
        }));
        assert!(events.contains(&SfuTopologyEvent::StandbyElected {
            standby_id: "node-b".to_owned()
        }));

        assert!(
            topology
                .establish_initial_host("node-b", Some("node-a"))
                .is_empty()
        );
        assert_eq!(topology.active_host(), Some("node-a"));
    }

    #[test]
    fn sfu_topology_heartbeat_timeout_promotes_standby() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        let nodes = [node("node1", 100.0), node("node2", 80.0)];

        topology.update(&nodes, 100);
        topology.record_heartbeat("node1", 100);
        topology.record_heartbeat("node2", 106);

        assert_eq!(topology.active_host(), Some("node1"));
        assert_eq!(topology.standby_host(), Some("node2"));

        // Host node1 times out after 10 seconds (timeout is 5s)
        let timeout_events = topology.check_heartbeat_timeout(110);
        assert!(timeout_events.contains(&SfuTopologyEvent::HostTimedOut {
            failed_host: "node1".into()
        }));
        assert_eq!(topology.active_host(), Some("node2"));
    }

    #[test]
    fn sfu_topology_does_not_promote_a_timed_out_standby() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        let nodes = [node("node1", 100.0), node("node2", 80.0)];

        topology.update(&nodes, 100);
        topology.record_heartbeat("node1", 100);
        topology.record_heartbeat("node2", 100);

        let timeout_events = topology.check_heartbeat_timeout(110);
        assert!(timeout_events.contains(&SfuTopologyEvent::HostTimedOut {
            failed_host: "node1".into()
        }));
        assert_eq!(topology.active_host(), None);
        assert_eq!(topology.standby_host(), None);
    }

    #[test]
    fn sfu_topology_make_before_break_migration() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        let nodes1 = [node("incumbent", 100.0), node("challenger", 30.0)];

        topology.update(&nodes1, 100);
        assert_eq!(topology.active_host(), Some("incumbent"));

        // Challenger capacity jumps significantly (> 25% improvement required)
        let nodes2 = [node("incumbent", 30.0), node("challenger", 150.0)];
        let events = topology.update(&nodes2, 101);

        assert!(events.contains(&SfuTopologyEvent::MigrationStarted {
            from: "incumbent".into(),
            to: "challenger".into(),
        }));

        assert!(matches!(
            topology.migration_state(),
            SfuMigrationState::Migrating { .. }
        ));

        // Confirm migration completion
        let completed_event = topology.confirm_migration();
        assert_eq!(
            completed_event,
            Some(SfuTopologyEvent::MigrationCompleted {
                new_host: "challenger".into()
            })
        );
        assert_eq!(topology.active_host(), Some("challenger"));
        assert_eq!(*topology.migration_state(), SfuMigrationState::Stable);
    }

    #[test]
    fn replica_does_not_start_capacity_migration_without_host_proposal() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new_convergent(policy);
        topology.update(&[node("node1", 25.0), node("node2", 200.0)], 100);
        let events =
            topology.update_with_role(&[node("node1", 25.0), node("node2", 200.0)], 101, false);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, SfuTopologyEvent::MigrationStarted { .. }))
        );
        assert_eq!(topology.active_host(), Some("node1"));

        let proposal = SfuMigrationProposal::new(2, "node1".into(), "node2".into())
            .expect("valid proposal should be created");
        assert!(topology.accept_migration(&proposal, 102));
        assert!(!topology.accept_migration(&proposal, 103));
        assert_eq!(topology.term(), 2);
    }

    #[test]
    fn removing_a_migration_target_does_not_leave_topology_stuck() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        topology.update(&[node("node1", 80.0), node("node2", 30.0)], 100);
        let events = topology.update(&[node("node1", 30.0), node("node2", 150.0)], 101);
        assert!(matches!(
            events.as_slice(),
            [SfuTopologyEvent::MigrationStarted { .. }]
        ));

        topology.remove_node("node2");
        assert_eq!(topology.migration_state(), &SfuMigrationState::Stable);
    }
}
