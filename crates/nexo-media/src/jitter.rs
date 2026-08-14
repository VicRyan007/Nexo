use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::EncodedAudioFrame;

const DEFAULT_PREBUFFER_PACKETS: u16 = 3;
const MAX_BUFFERED_PACKETS: usize = 50;
const FRAME_DURATION: Duration = Duration::from_millis(20);
const MAX_CONSECUTIVE_CONCEALMENTS: u8 = 10;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlayoutFrame {
    Packet(EncodedAudioFrame),
    Loss {
        recovery_packet: Option<EncodedAudioFrame>,
    },
}

pub struct JitterBuffer {
    packets: BTreeMap<u64, EncodedAudioFrame>,
    expected: Option<u64>,
    highest: Option<u64>,
    started: bool,
    prebuffer_packets: u16,
    next_playout: Option<Instant>,
    consecutive_concealments: u8,
    waiting_for_restart: bool,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new(DEFAULT_PREBUFFER_PACKETS)
    }
}

impl JitterBuffer {
    #[must_use]
    pub fn new(prebuffer_packets: u16) -> Self {
        Self {
            packets: BTreeMap::new(),
            expected: None,
            highest: None,
            started: false,
            prebuffer_packets: prebuffer_packets.max(1),
            next_playout: None,
            consecutive_concealments: 0,
            waiting_for_restart: false,
        }
    }

    pub fn push(&mut self, sequence: u16, frame: EncodedAudioFrame) -> bool {
        let extended = self.extend_sequence(sequence);
        if self.waiting_for_restart {
            self.packets.clear();
            self.expected = Some(extended);
            self.highest = Some(extended);
            self.started = false;
            self.next_playout = None;
            self.consecutive_concealments = 0;
            self.waiting_for_restart = false;
            self.packets.insert(extended, frame);
            return true;
        }
        if self.expected.is_some_and(|expected| extended < expected)
            || self.packets.contains_key(&extended)
        {
            return false;
        }
        if self.expected.is_none() {
            self.expected = Some(extended);
        }
        self.highest = Some(
            self.highest
                .map_or(extended, |highest| highest.max(extended)),
        );
        self.packets.insert(extended, frame);
        while self.packets.len() > MAX_BUFFERED_PACKETS {
            let Some(oldest) = self.packets.first_key_value().map(|(&key, _)| key) else {
                break;
            };
            self.packets.remove(&oldest);
            if self.expected.is_some_and(|expected| expected <= oldest) {
                self.expected = Some(oldest + 1);
            }
        }
        true
    }

    pub fn pop_ready_at(&mut self, now: Instant) -> Option<PlayoutFrame> {
        let expected = self.expected?;
        self.highest?;
        if !self.started {
            if self.packets.len() < usize::from(self.prebuffer_packets) {
                return None;
            }
            self.started = true;
            self.next_playout = Some(now);
        }
        let deadline = self.next_playout?;
        if now < deadline {
            return None;
        }
        self.next_playout = Some(deadline + FRAME_DURATION);
        if let Some(frame) = self.packets.remove(&expected) {
            self.expected = Some(expected + 1);
            self.consecutive_concealments = 0;
            return Some(PlayoutFrame::Packet(frame));
        }
        let recovery_packet = self.packets.get(&(expected + 1)).cloned();
        if recovery_packet.is_none() {
            if self.consecutive_concealments >= MAX_CONSECUTIVE_CONCEALMENTS {
                self.waiting_for_restart = true;
                self.started = false;
                self.next_playout = None;
                self.packets.clear();
                return None;
            }
            self.consecutive_concealments += 1;
        } else {
            self.consecutive_concealments = 0;
        }
        self.expected = Some(expected + 1);
        Some(PlayoutFrame::Loss { recovery_packet })
    }

    #[must_use]
    pub fn buffered_packets(&self) -> usize {
        self.packets.len()
    }

    fn extend_sequence(&self, sequence: u16) -> u64 {
        let Some(reference) = self.highest else {
            return u64::from(sequence);
        };
        let cycle = reference & !u64::from(u16::MAX);
        let candidate = cycle | u64::from(sequence);
        let half_range = 1_u64 << 15;
        let full_range = 1_u64 << 16;
        if candidate.saturating_add(half_range) < reference {
            candidate.saturating_add(full_range)
        } else if candidate > reference.saturating_add(half_range) {
            candidate.saturating_sub(full_range)
        } else {
            candidate
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u8) -> EncodedAudioFrame {
        EncodedAudioFrame {
            payload: vec![value],
            sample_count: 960,
            sample_rate: 48_000,
        }
    }

    #[test]
    fn reorders_packets_after_small_prebuffer() {
        let mut jitter = JitterBuffer::new(3);
        let start = Instant::now();
        assert!(jitter.push(10, frame(10)));
        assert!(jitter.push(12, frame(12)));
        assert!(jitter.pop_ready_at(start).is_none());
        assert!(jitter.push(11, frame(11)));
        for (offset, expected) in [(0, 10), (1, 11), (2, 12)] {
            assert_eq!(
                jitter.pop_ready_at(start + FRAME_DURATION * offset),
                Some(PlayoutFrame::Packet(frame(expected)))
            );
        }
    }

    #[test]
    fn confirms_loss_only_after_later_packets_arrive() {
        let mut jitter = JitterBuffer::new(3);
        let start = Instant::now();
        jitter.push(20, frame(20));
        jitter.push(22, frame(22));
        jitter.push(23, frame(23));
        assert_eq!(
            jitter.pop_ready_at(start),
            Some(PlayoutFrame::Packet(frame(20)))
        );
        assert_eq!(
            jitter.pop_ready_at(start + FRAME_DURATION),
            Some(PlayoutFrame::Loss {
                recovery_packet: Some(frame(22))
            })
        );
        assert_eq!(
            jitter.pop_ready_at(start + FRAME_DURATION * 2),
            Some(PlayoutFrame::Packet(frame(22)))
        );
    }

    #[test]
    fn handles_sequence_wrap_and_rejects_late_duplicates() {
        let mut jitter = JitterBuffer::new(2);
        let start = Instant::now();
        assert!(jitter.push(u16::MAX, frame(1)));
        assert!(jitter.push(0, frame(2)));
        assert_eq!(
            jitter.pop_ready_at(start),
            Some(PlayoutFrame::Packet(frame(1)))
        );
        assert!(!jitter.push(u16::MAX, frame(1)));
        assert_eq!(
            jitter.pop_ready_at(start + FRAME_DURATION),
            Some(PlayoutFrame::Packet(frame(2)))
        );
    }

    #[test]
    fn playout_is_clocked_instead_of_draining_a_network_burst() {
        let mut jitter = JitterBuffer::new(2);
        let start = Instant::now();
        jitter.push(30, frame(30));
        jitter.push(31, frame(31));
        assert_eq!(
            jitter.pop_ready_at(start),
            Some(PlayoutFrame::Packet(frame(30)))
        );
        assert!(jitter.pop_ready_at(start).is_none());
        assert!(
            jitter
                .pop_ready_at(start + Duration::from_millis(19))
                .is_none()
        );
        assert_eq!(
            jitter.pop_ready_at(start + FRAME_DURATION),
            Some(PlayoutFrame::Packet(frame(31)))
        );
    }

    #[test]
    fn stalled_stream_has_bounded_concealment_and_restarts_cleanly() {
        let mut jitter = JitterBuffer::new(1);
        let start = Instant::now();
        jitter.push(100, frame(100));
        assert!(matches!(
            jitter.pop_ready_at(start),
            Some(PlayoutFrame::Packet(_))
        ));
        for interval in 1..=MAX_CONSECUTIVE_CONCEALMENTS {
            assert_eq!(
                jitter.pop_ready_at(start + FRAME_DURATION * u32::from(interval)),
                Some(PlayoutFrame::Loss {
                    recovery_packet: None
                })
            );
        }
        assert!(
            jitter
                .pop_ready_at(start + FRAME_DURATION * u32::from(MAX_CONSECUTIVE_CONCEALMENTS + 1),)
                .is_none()
        );
        assert!(jitter.push(500, frame(5)));
        assert_eq!(
            jitter
                .pop_ready_at(start + FRAME_DURATION * u32::from(MAX_CONSECUTIVE_CONCEALMENTS + 2)),
            Some(PlayoutFrame::Packet(frame(5)))
        );
    }
}
