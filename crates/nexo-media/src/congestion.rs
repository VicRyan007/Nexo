//! Adaptive Bitrate and Congestion Control for real-time video/audio streaming.
//!
//! Evaluates round-trip time (RTT), packet loss fraction, and jitter to dynamically
//! adjust video encoding parameters (target bitrate, target FPS, and resolution tier).

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::time::{Duration, Instant};

/// Video quality profile determined by network capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoQualityProfile {
    pub target_bitrate_kbps: u32,
    pub target_fps: u32,
    pub width: u32,
    pub height: u32,
}

/// Instantaneous network health metrics measured from transport feedback.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NetworkMetrics {
    pub rtt: Duration,
    pub packet_loss_ratio: f32,
    pub jitter: Duration,
}

impl Default for VideoQualityProfile {
    fn default() -> Self {
        Self {
            target_bitrate_kbps: 1200,
            target_fps: 30,
            width: 1280,
            height: 720,
        }
    }
}

/// Congestion controller executing AIMD (Additive Increase / Multiplicative Decrease)
/// adaptation for real-time WebRTC media streams.
#[derive(Debug)]
pub struct CongestionController {
    current_profile: VideoQualityProfile,
    min_bitrate_kbps: u32,
    max_bitrate_kbps: u32,
    last_adjustment: Instant,
    consecutive_good_reports: u32,
}

impl Default for CongestionController {
    fn default() -> Self {
        Self::new(VideoQualityProfile::default(), 150, 3000)
    }
}

impl CongestionController {
    #[must_use]
    pub fn new(
        initial_profile: VideoQualityProfile,
        min_bitrate_kbps: u32,
        max_bitrate_kbps: u32,
    ) -> Self {
        Self {
            current_profile: initial_profile,
            min_bitrate_kbps,
            max_bitrate_kbps,
            last_adjustment: Instant::now(),
            consecutive_good_reports: 0,
        }
    }

    #[must_use]
    pub const fn current_profile(&self) -> VideoQualityProfile {
        self.current_profile
    }

    pub(crate) fn restore_profile(&mut self, profile: VideoQualityProfile) {
        self.current_profile = profile;
        self.consecutive_good_reports = 0;
        self.last_adjustment = Instant::now();
    }

    /// Process updated transport metrics and compute new target quality profile.
    pub fn on_network_metrics(
        &mut self,
        metrics: NetworkMetrics,
        now: Instant,
    ) -> VideoQualityProfile {
        // Enforce cooldown between adjustments (min 500ms)
        if now.duration_since(self.last_adjustment) < Duration::from_millis(500) {
            return self.current_profile;
        }

        let is_congested =
            metrics.packet_loss_ratio > 0.08 || metrics.rtt > Duration::from_millis(250);
        let is_degraded =
            metrics.packet_loss_ratio > 0.03 || metrics.rtt > Duration::from_millis(150);

        if is_congested {
            // Multiplicative decrease (drop by 25%) and lower framerate
            self.consecutive_good_reports = 0;
            let new_bitrate = (self.current_profile.target_bitrate_kbps as f32 * 0.75) as u32;
            self.current_profile.target_bitrate_kbps = new_bitrate.max(self.min_bitrate_kbps);
            self.current_profile.target_fps = 15;
            if self.current_profile.target_bitrate_kbps <= 400 {
                self.current_profile.width = 640;
                self.current_profile.height = 360;
            }
            self.last_adjustment = now;
        } else if is_degraded {
            // Slight decrease (drop by 10%)
            self.consecutive_good_reports = 0;
            let new_bitrate = (self.current_profile.target_bitrate_kbps as f32 * 0.90) as u32;
            self.current_profile.target_bitrate_kbps = new_bitrate.max(self.min_bitrate_kbps);
            self.current_profile.target_fps = 20;
            self.last_adjustment = now;
        } else {
            // Stable link: Additive increase after consecutive good reports
            self.consecutive_good_reports += 1;
            if self.consecutive_good_reports >= 2 {
                self.consecutive_good_reports = 0;
                let new_bitrate = self.current_profile.target_bitrate_kbps.saturating_add(100);
                self.current_profile.target_bitrate_kbps = new_bitrate.min(self.max_bitrate_kbps);
                if self.current_profile.target_bitrate_kbps >= 1000 {
                    self.current_profile.target_fps = 30;
                    self.current_profile.width = 1280;
                    self.current_profile.height = 720;
                } else if self.current_profile.target_bitrate_kbps >= 600 {
                    self.current_profile.target_fps = 24;
                    self.current_profile.width = 854;
                    self.current_profile.height = 480;
                }
                self.last_adjustment = now;
            }
        }

        self.current_profile
    }

    /// Select a bounded quality tier from the receiver's available bitrate.
    ///
    /// REMB is the only transport signal guaranteed by every supported WebRTC
    /// backend. It is therefore used as a conservative input for the live
    /// encoder profile; packet loss and RTT can still refine the profile through
    /// [`Self::on_network_metrics`].
    pub fn on_bitrate_estimate(&mut self, bitrate_bps: u32, now: Instant) -> VideoQualityProfile {
        if now.duration_since(self.last_adjustment) < Duration::from_millis(500) {
            return self.current_profile;
        }

        let estimate_kbps = bitrate_bps / 1_000;
        let tier = if estimate_kbps < 500 {
            VideoQualityProfile {
                target_bitrate_kbps: self.min_bitrate_kbps,
                target_fps: 15,
                width: 640,
                height: 360,
            }
        } else if estimate_kbps < 1_000 {
            VideoQualityProfile {
                target_bitrate_kbps: 600.max(self.min_bitrate_kbps),
                target_fps: 24,
                width: 854,
                height: 480,
            }
        } else {
            VideoQualityProfile {
                target_bitrate_kbps: 1_200.min(self.max_bitrate_kbps),
                target_fps: 30,
                width: 1_280,
                height: 720,
            }
        };

        if tier != self.current_profile {
            self.current_profile = tier;
            self.consecutive_good_reports = 0;
            self.last_adjustment = now;
        }
        self.current_profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn congestion_controller_reduces_on_loss_and_recovers_on_clean_link() {
        let mut controller = CongestionController::new(
            VideoQualityProfile {
                target_bitrate_kbps: 1500,
                target_fps: 30,
                width: 1280,
                height: 720,
            },
            150,
            2500,
        );

        let mut now = Instant::now();

        // 1. Congested report (15% loss) -> multiplicative decrease
        now += Duration::from_millis(600);
        let profile = controller.on_network_metrics(
            NetworkMetrics {
                rtt: Duration::from_millis(300),
                packet_loss_ratio: 0.15,
                jitter: Duration::from_millis(30),
            },
            now,
        );
        assert!(profile.target_bitrate_kbps < 1500);
        assert_eq!(profile.target_fps, 15);

        // 2. Continuous clean reports -> gradual recovery
        for _ in 0..10 {
            now += Duration::from_millis(600);
            controller.on_network_metrics(
                NetworkMetrics {
                    rtt: Duration::from_millis(20),
                    packet_loss_ratio: 0.0,
                    jitter: Duration::from_millis(2),
                },
                now,
            );
        }
        let recovered = controller.current_profile();
        assert!(recovered.target_bitrate_kbps > profile.target_bitrate_kbps);
        assert_eq!(recovered.target_fps, 30);
    }

    #[test]
    fn bitrate_estimate_selects_low_medium_and_high_quality_tiers() {
        let initial = VideoQualityProfile {
            target_bitrate_kbps: 1_500,
            target_fps: 30,
            width: 640,
            height: 480,
        };
        let mut controller = CongestionController::new(initial, 150, 3_000);
        let mut now = Instant::now() + Duration::from_millis(600);

        let low = controller.on_bitrate_estimate(300_000, now);
        assert_eq!((low.target_fps, low.width, low.height), (15, 640, 360));

        now += Duration::from_millis(600);
        let medium = controller.on_bitrate_estimate(800_000, now);
        assert_eq!(
            (medium.target_fps, medium.width, medium.height),
            (24, 854, 480)
        );

        now += Duration::from_millis(600);
        let high = controller.on_bitrate_estimate(2_000_000, now);
        assert_eq!((high.target_fps, high.width, high.height), (30, 1_280, 720));
    }
}
