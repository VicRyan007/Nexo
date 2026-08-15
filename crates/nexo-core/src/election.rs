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
}

impl SfuTopology {
    #[must_use]
    pub fn new(policy: ElectionPolicy) -> Self {
        Self {
            active_host: None,
            standby_host: None,
            migration_state: SfuMigrationState::Stable,
            last_heartbeat: HashMap::new(),
            policy,
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

    #[must_use]
    pub fn migration_state(&self) -> &SfuMigrationState {
        &self.migration_state
    }

    pub fn record_heartbeat(&mut self, node_id: &str, timestamp: u64) {
        self.last_heartbeat.insert(node_id.to_owned(), timestamp);
    }

    /// Evaluates current metrics and updates the host and standby selections.
    /// Returns events triggered by election or migration changes.
    pub fn update(&mut self, nodes: &[NodeMetrics], now: u64) -> Vec<SfuTopologyEvent> {
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
        let second_candidate = eligible.get(1).copied();

        match (self.active_host.as_deref(), top_candidate) {
            (None, Some(candidate)) => {
                let host_id = candidate.node_id.clone();
                self.active_host = Some(host_id.clone());
                events.push(SfuTopologyEvent::HostElected {
                    host_id: host_id.clone(),
                });

                if let Some(standby) = second_candidate {
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

                let should_switch = if let Some(incumbent_node) = incumbent {
                    if candidate.node_id == incumbent_node.node_id {
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
                let standby_candidate =
                    eligible.iter().find(|n| n.node_id != incumbent_id).copied();
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

                if let Some(standby_id) = self.standby_host.take() {
                    self.active_host = Some(standby_id.clone());
                    events.push(SfuTopologyEvent::HostElected {
                        host_id: standby_id,
                    });
                } else {
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
    fn sfu_topology_heartbeat_timeout_promotes_standby() {
        let policy = ElectionPolicy::default();
        let mut topology = SfuTopology::new(policy);
        let nodes = [node("node1", 100.0), node("node2", 80.0)];

        topology.update(&nodes, 100);
        topology.record_heartbeat("node1", 100);
        topology.record_heartbeat("node2", 100);

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
}
