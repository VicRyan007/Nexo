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

#[derive(Clone, Copy, Debug)]
pub struct ElectionPolicy {
    pub minimum_upload_mbps: f32,
    pub maximum_packet_loss_percent: f32,
    pub switch_improvement_percent: f32,
}

impl Default for ElectionPolicy {
    fn default() -> Self {
        Self {
            minimum_upload_mbps: 20.0,
            maximum_packet_loss_percent: 8.0,
            switch_improvement_percent: 25.0,
        }
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
}
