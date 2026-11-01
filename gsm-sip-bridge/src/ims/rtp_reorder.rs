//! A bounded wait for a missing RTP sequence before carrier-side decoding.
//!
//! Packets retain their complete original headers and payloads. This buffer
//! only changes delivery order; it has no playout clock or codec knowledge.

use super::rtp;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub(super) struct RtpReorderBuffer {
    max_wait: Duration,
    ssrc: Option<u32>,
    expected: u16,
    pending: BTreeMap<u16, Vec<u8>>,
    deadline: Option<Instant>,
}

impl RtpReorderBuffer {
    pub(super) fn new(max_wait: Duration) -> Self {
        Self {
            max_wait,
            ssrc: None,
            expected: 0,
            pending: BTreeMap::new(),
            deadline: None,
        }
    }

    /// Delivers the first packet immediately. Later packets wait only when
    /// their sequence is ahead of the next expected one. Receiving that
    /// expected packet releases the entire batch in order, skipping any
    /// other holes. An expired wait does the same without the missing packet.
    pub(super) fn push(&mut self, packet: Vec<u8>, now: Instant) -> Vec<Vec<u8>> {
        let Some(parsed) = rtp::parse_packet(&packet) else {
            return self.flush_expired(now);
        };
        let (seq, ssrc) = (parsed.seq, parsed.ssrc);

        if self.ssrc != Some(ssrc) {
            // A new source owns a new sequence space. Old-source packets
            // must not enter its decoder after the reset.
            self.pending.clear();
            self.deadline = None;
            self.ssrc = Some(ssrc);
            self.expected = seq.wrapping_add(1);
            return vec![packet];
        }

        // Apply the deadline before the new arrival: a packet that arrives
        // at or after expiry cannot fill a hole already skipped by the batch.
        let mut ready = self.flush_expired(now);
        let distance = seq.wrapping_sub(self.expected);
        if distance >= 0x8000 {
            return ready; // Already delivered, skipped, or otherwise too old.
        }
        if distance == 0 {
            self.expected = seq.wrapping_add(1);
            ready.push(packet);
            ready.extend(self.release_pending());
        } else {
            self.pending.entry(seq).or_insert(packet);
            // More future packets and duplicates cannot extend this wait.
            self.deadline.get_or_insert(now + self.max_wait);
            ready.extend(self.flush_expired(now));
        }
        ready
    }

    /// Allows the receive loop to release a batch even when no more packets
    /// arrive. The caller must wake at `deadline()` rather than await traffic.
    pub(super) fn flush_expired(&mut self, now: Instant) -> Vec<Vec<u8>> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.release_pending()
        } else {
            Vec::new()
        }
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    fn release_pending(&mut self) -> Vec<Vec<u8>> {
        self.deadline = None;
        let mut packets: Vec<_> = std::mem::take(&mut self.pending).into_iter().collect();
        // Numeric sorting alone would put 0 before 65535 at sequence wrap.
        packets.sort_unstable_by_key(|(seq, _)| seq.wrapping_sub(self.expected));
        if let Some((last, _)) = packets.last() {
            self.expected = last.wrapping_add(1);
        }
        packets.into_iter().map(|(_, packet)| packet).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAIT: Duration = Duration::from_millis(40);

    fn packet(seq: u16) -> Vec<u8> {
        rtp::build_packet(seq, seq as u32 * 160, 42, 96, &[0xf0, 0x04, 0x01])
    }

    fn seqs(packets: Vec<Vec<u8>>) -> Vec<u16> {
        packets
            .iter()
            .map(|packet| rtp::parse_packet(packet).unwrap().seq)
            .collect()
    }

    #[test]
    fn in_order_packets_are_immediate_and_unchanged() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        for seq in [10, 11, 12] {
            let original = packet(seq);
            assert_eq!(buffer.push(original.clone(), now), vec![original]);
            assert_eq!(buffer.deadline(), None);
        }
    }

    #[test]
    fn expected_packet_releases_permutation_in_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert_eq!(seqs(buffer.push(packet(10), now)), [10]);
        assert!(buffer.push(packet(14), now).is_empty());
        assert!(buffer.push(packet(12), now).is_empty());
        assert!(buffer.push(packet(13), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(11), now)), [11, 12, 13, 14]);
        assert_eq!(buffer.deadline(), None);
    }

    #[test]
    fn expected_packet_releases_all_buffered_packets_and_skips_other_holes() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(15), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(packet(11), now)), [11, 12, 15]);
        assert!(buffer.push(packet(13), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(16), now)), [16]);
    }

    #[test]
    fn timeout_releases_all_packets_without_another_arrival() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(15), now);
        buffer.push(packet(12), now + Duration::from_millis(10));
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert!(buffer
            .flush_expired(now + WAIT - Duration::from_nanos(1))
            .is_empty());
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [12, 15]);
        assert_eq!(buffer.deadline(), None);
        assert!(buffer.push(packet(11), now + WAIT).is_empty());
        assert_eq!(seqs(buffer.push(packet(16), now + WAIT)), [16]);
    }

    #[test]
    fn arrival_at_deadline_cannot_recover_the_missing_packet() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(packet(11), now + WAIT)), [12]);
    }

    #[test]
    fn duplicate_and_late_packets_are_discarded_without_extending_wait() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(10), now);
        assert!(buffer.push(packet(10), now).is_empty());
        buffer.push(packet(12), now);
        assert!(buffer
            .push(packet(12), now + Duration::from_millis(20))
            .is_empty());
        assert_eq!(buffer.deadline(), Some(now + WAIT));
        assert_eq!(
            seqs(buffer.push(packet(11), now + Duration::from_millis(30))),
            [11, 12]
        );
        assert!(buffer
            .push(packet(12), now + Duration::from_millis(31))
            .is_empty());
    }

    #[test]
    fn sequence_wrap_preserves_modular_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert_eq!(seqs(buffer.push(packet(65533), now)), [65533]);
        buffer.push(packet(1), now);
        buffer.push(packet(0), now);
        buffer.push(packet(65535), now);
        assert_eq!(seqs(buffer.push(packet(65534), now)), [65534, 65535, 0, 1]);
        assert!(buffer.push(packet(65535), now).is_empty());
        assert_eq!(seqs(buffer.push(packet(2), now)), [2]);
    }

    #[test]
    fn timeout_at_sequence_wrap_preserves_modular_order() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(65533), now);
        buffer.push(packet(1), now);
        buffer.push(packet(65535), now);
        assert_eq!(seqs(buffer.flush_expired(now + WAIT)), [65535, 1]);
        assert_eq!(seqs(buffer.push(packet(2), now + WAIT)), [2]);
    }

    #[test]
    fn ssrc_change_resets_sequence_and_discards_pending_source() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        buffer.push(packet(100), now);
        buffer.push(packet(102), now);
        let new_source = rtp::build_packet(1, 7, 99, 96, &[0xf0, 0x04, 0x01]);
        assert_eq!(buffer.push(new_source.clone(), now), [new_source]);
        assert_eq!(buffer.deadline(), None);
        assert!(buffer.flush_expired(now + WAIT).is_empty());
        assert_eq!(
            seqs(buffer.push(rtp::build_packet(2, 167, 99, 96, &[]), now)),
            [2]
        );
    }

    #[test]
    fn zero_wait_skips_missing_packets_immediately() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(Duration::ZERO);
        assert_eq!(seqs(buffer.push(packet(10), now)), [10]);
        assert_eq!(seqs(buffer.push(packet(12), now)), [12]);
        assert!(buffer.push(packet(11), now).is_empty());
        assert_eq!(buffer.deadline(), None);
    }

    #[test]
    fn invalid_rtp_cannot_reset_stream_or_block_timeout() {
        let now = Instant::now();
        let mut buffer = RtpReorderBuffer::new(WAIT);
        assert!(buffer.push(vec![0; 12], now).is_empty());
        buffer.push(packet(10), now);
        buffer.push(packet(12), now);
        assert_eq!(seqs(buffer.push(vec![0; 12], now + WAIT)), [12]);
        assert_eq!(seqs(buffer.push(packet(13), now + WAIT)), [13]);
    }
}
