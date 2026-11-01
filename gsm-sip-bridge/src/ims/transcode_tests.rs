//! UDP-level regression tests: ordering must happen before the stateful AMR
//! decoder, and the original downlink RTP timeline must survive transcoding.
#![cfg(feature = "amr-linked")]

use super::*;
use std::time::Instant;

const SOURCE_SSRC: u32 = 0x1234_abcd;
const REORDER_WAIT: Duration = Duration::from_millis(40);

fn codec(codec: NegotiatedCodec, payload_type: u8) -> ChosenCodec {
    ChosenCodec {
        codec,
        payload_type,
        octet_aligned: true,
        dtmf_payload_type: None,
    }
}

fn connected_pair() -> (UdpSocket, UdpSocket) {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    a.connect(b.local_addr().unwrap()).unwrap();
    b.connect(a.local_addr().unwrap()).unwrap();
    (a, b)
}

struct Relay {
    carrier: UdpSocket,
    pbx: UdpSocket,
    stop: Arc<AtomicBool>,
}

impl Relay {
    fn start(carrier_codec: ChosenCodec, pbx_codec: ChosenCodec, wait: Duration) -> Self {
        let (ims, carrier) = connected_pair();
        let (veth, pbx) = connected_pair();
        for socket in [&carrier, &pbx] {
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
        }
        let stop = Arc::new(AtomicBool::new(false));
        spawn_transcoding_relay(
            ims,
            veth,
            carrier_codec,
            pbx_codec,
            stop.clone(),
            &super::super::media_stats::MediaMeter::new(),
            None,
            wait,
        )
        .unwrap();
        Self { carrier, pbx, stop }
    }

    fn downlink(&self, packet: &[u8]) {
        self.carrier.send(packet).unwrap();
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Received {
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload_type: u8,
    marker: bool,
    payload: Vec<u8>,
}

fn receive(socket: &UdpSocket) -> Received {
    let mut buf = [0u8; RECV_BUF];
    let n = socket
        .recv(&mut buf)
        .expect("relay must emit an RTP packet");
    let packet = rtp::parse_packet(&buf[..n]).unwrap();
    Received {
        seq: packet.seq,
        timestamp: packet.timestamp,
        ssrc: packet.ssrc,
        payload_type: packet.payload_type,
        marker: buf[1] & 0x80 != 0,
        payload: packet.payload.to_vec(),
    }
}

fn assert_no_packet(socket: &UdpSocket, timeout: Duration) {
    socket.set_read_timeout(Some(timeout)).unwrap();
    let error = socket.recv(&mut [0u8; RECV_BUF]).unwrap_err();
    assert!(matches!(
        error.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
    ));
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
}

fn source_packet(seq: u16, timestamp: u32, payload_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut packet = rtp::build_packet(seq, timestamp, SOURCE_SSRC, payload_type, payload);
    if seq.is_multiple_of(2) {
        packet[1] |= 0x80;
    }
    packet
}

/// Continuous speech-like input gives every AMR frame its own content and
/// history; decoding frames in arrival order cannot accidentally pass by
/// merely sorting their RTP headers afterwards.
fn speech_payloads(kind: AmrKind, octet_aligned: bool, count: usize) -> Vec<Vec<u8>> {
    let mut encoder = match kind {
        AmrKind::Nb => AmrEncoder::Nb(amr_safe::NbEncoder::new().unwrap()),
        AmrKind::Wb => AmrEncoder::Wb(amr_safe::WbEncoder::new().unwrap()),
    };
    let samples = match kind {
        AmrKind::Nb => amr_safe::NB_FRAME_SAMPLES,
        AmrKind::Wb => amr_safe::FRAME_SAMPLES,
    };
    (0..count)
        .map(|frame_index| {
            let pcm: Vec<i16> = (0..samples)
                .map(|sample_index| {
                    let position = (frame_index * samples + sample_index) as f64;
                    let frequency = 0.08 + frame_index as f64 * 0.013;
                    (position.mul_add(frequency, 0.0).sin() * 12_000.0) as i16
                })
                .collect();
            let frame = encoder.encode(&pcm).unwrap();
            amr_rtp::frame_to_payload(&frame, kind, octet_aligned).unwrap()
        })
        .collect()
}

fn check_shuffled_amr(kind: AmrKind, octet_aligned: bool) {
    let (carrier_codec, pbx_codec) = match kind {
        AmrKind::Nb => (NegotiatedCodec::AmrNb, NegotiatedCodec::Pcmu),
        AmrKind::Wb => (NegotiatedCodec::AmrWb, NegotiatedCodec::L16),
    };
    let mut carrier = codec(carrier_codec, 97);
    carrier.octet_aligned = octet_aligned;
    let pbx = codec(pbx_codec, 96);
    let timestamp_step = carrier_codec.frame_samples() as u32;
    let packets: Vec<_> = speech_payloads(kind, octet_aligned, 5)
        .iter()
        .enumerate()
        .map(|(i, payload)| {
            source_packet(
                700 + i as u16,
                8_500 + i as u32 * timestamp_step,
                97,
                payload,
            )
        })
        .collect();

    let ordered = Relay::start(carrier, pbx, REORDER_WAIT);
    let expected: Vec<_> = packets
        .iter()
        .map(|packet| {
            ordered.downlink(packet);
            receive(&ordered.pbx)
        })
        .collect();
    let shuffled = Relay::start(carrier, pbx, REORDER_WAIT);
    shuffled.downlink(&packets[0]);
    assert_eq!(receive(&shuffled.pbx), expected[0]);
    for index in [3, 2, 1, 4] {
        shuffled.downlink(&packets[index]);
    }
    for (index, expected) in expected.iter().enumerate().skip(1) {
        let actual = receive(&shuffled.pbx);
        assert_eq!(
            &actual, expected,
            "AMR frame {index} must decode in source order"
        );
        assert_eq!(actual.seq, 700 + index as u16);
        assert_eq!(actual.timestamp, 8_500 + index as u32 * timestamp_step);
        assert_eq!(actual.ssrc, SOURCE_SSRC);
        assert_eq!(actual.payload_type, 96);
        assert_eq!(actual.marker, actual.seq.is_multiple_of(2));
    }
}

#[test]
fn amr_nb_is_reordered_before_decoding_in_both_payload_formats() {
    check_shuffled_amr(AmrKind::Nb, true);
    check_shuffled_amr(AmrKind::Nb, false);
}

#[test]
fn amr_wb_is_reordered_before_decoding_in_both_payload_formats() {
    check_shuffled_amr(AmrKind::Wb, true);
    check_shuffled_amr(AmrKind::Wb, false);
}

#[test]
fn missing_packet_expires_without_further_input_and_late_packets_never_reach_decoder() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 6)
        .iter()
        .enumerate()
        .map(|(i, payload)| source_packet(10 + i as u16, 900 + i as u32 * 160, 97, payload))
        .collect();
    let baseline = Relay::start(carrier, pbx, Duration::ZERO);
    let expected: Vec<_> = [0, 2, 4, 5]
        .into_iter()
        .map(|index| {
            baseline.downlink(&packets[index]);
            receive(&baseline.pbx)
        })
        .collect();

    let relay = Relay::start(carrier, pbx, REORDER_WAIT);
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);
    let sent_at = Instant::now();
    relay.downlink(&packets[2]);
    relay.downlink(&packets[4]);
    // The decoder cannot see frame 2 while frame 1 can still arrive.
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    let emitted = receive(&relay.pbx);
    assert!(sent_at.elapsed() >= Duration::from_millis(30));
    assert!(sent_at.elapsed() < Duration::from_millis(500));
    assert_eq!(emitted, expected[1]);
    assert_eq!((emitted.seq, emitted.timestamp), (12, 1_220));
    // A second hole in the accumulated batch does not start another wait.
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    assert_eq!(receive(&relay.pbx), expected[2]);
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();

    // Both a packet whose deadline passed and a duplicate of an already
    // emitted packet must be ignored before they can mutate decoder state.
    relay.downlink(&packets[1]);
    relay.downlink(&packets[2]);
    relay.downlink(&packets[3]);
    relay.downlink(&packets[5]);
    assert_eq!(receive(&relay.pbx), expected[3]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn zero_wait_skips_immediately_and_discards_late_or_duplicate_amr_before_decoding() {
    let carrier = codec(NegotiatedCodec::AmrNb, 97);
    let pbx = codec(NegotiatedCodec::Pcmu, 0);
    let packets: Vec<_> = speech_payloads(AmrKind::Nb, true, 4)
        .iter()
        .enumerate()
        .map(|(i, payload)| source_packet(10 + i as u16, 900 + i as u32 * 160, 97, payload))
        .collect();
    let baseline = Relay::start(carrier, pbx, Duration::ZERO);
    let expected: Vec<_> = [0, 2, 3]
        .into_iter()
        .map(|index| {
            baseline.downlink(&packets[index]);
            receive(&baseline.pbx)
        })
        .collect();
    let relay = Relay::start(carrier, pbx, Duration::ZERO);
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    relay.downlink(&packets[0]);
    assert_eq!(receive(&relay.pbx), expected[0]);
    relay.downlink(&packets[2]);
    assert_eq!(receive(&relay.pbx), expected[1]);
    relay.downlink(&packets[1]);
    relay.downlink(&packets[2]);
    relay.downlink(&packets[3]);
    assert_eq!(receive(&relay.pbx), expected[2]);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn an_interleaved_dtmf_packet_completes_the_shared_sequence_space() {
    let mut carrier = codec(NegotiatedCodec::AmrNb, 97);
    carrier.dtmf_payload_type = Some(100);
    let mut pbx = codec(NegotiatedCodec::Pcmu, 0);
    pbx.dtmf_payload_type = Some(101);
    // Long enough that a receive timeout can distinguish releasing on DTMF
    // arrival from incorrectly waiting for an absent audio sequence 101.
    let relay = Relay::start(carrier, pbx, Duration::from_millis(500));
    let payloads = speech_payloads(AmrKind::Nb, true, 2);
    relay.downlink(&source_packet(100, 1_000, 97, &payloads[0]));
    receive(&relay.pbx);
    relay.downlink(&source_packet(102, 1_320, 97, &payloads[1]));
    assert_no_packet(&relay.pbx, Duration::from_millis(10));
    relay
        .pbx
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let event = [1, 10, 0, 160];
    relay.downlink(&source_packet(101, 1_000, 100, &event));
    let first = receive(&relay.pbx);
    let second = receive(&relay.pbx);
    let audio = if first.payload_type == 0 {
        assert_eq!(second.payload_type, 101);
        assert_eq!(second.payload, event);
        first
    } else {
        assert_eq!(first.payload_type, 101);
        assert_eq!(first.payload, event);
        second
    };
    assert_eq!((audio.seq, audio.timestamp), (102, 1_320));
}

#[test]
fn no_data_and_timestamp_wrap_preserve_the_original_timeline() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrNb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        REORDER_WAIT,
    );
    let payloads = speech_payloads(AmrKind::Nb, true, 2);
    let initial_timestamp = u32::MAX - 79;
    relay.downlink(&source_packet(65_534, initial_timestamp, 97, &payloads[0]));
    let first = receive(&relay.pbx);
    assert_eq!(first.timestamp, initial_timestamp);
    // Octet-aligned CMR=15, FT=15 (NO_DATA), Q=1.
    relay.downlink(&source_packet(
        65_535,
        initial_timestamp.wrapping_add(160),
        97,
        &[0xf0, 0x7c],
    ));
    relay.downlink(&source_packet(
        0,
        initial_timestamp.wrapping_add(320),
        97,
        &payloads[1],
    ));
    let next = receive(&relay.pbx);
    assert_eq!(next.seq, 0);
    assert_eq!(next.timestamp, initial_timestamp.wrapping_add(320));
    assert_eq!(next.ssrc, SOURCE_SSRC);
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn wideband_to_pcmu_scales_the_source_clock_continuously_through_wrap_and_no_data() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrWb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        REORDER_WAIT,
    );
    let payloads = speech_payloads(AmrKind::Wb, true, 3);
    let source_start = u32::MAX - 159;
    let output_start = source_start / 2;
    relay.downlink(&source_packet(200, source_start, 97, &payloads[0]));
    let first = receive(&relay.pbx);
    assert_eq!((first.seq, first.timestamp), (200, output_start));
    assert_eq!(first.payload.len(), 160);

    // Both source timestamps wrap, but the destination clock must continue
    // from its own scaled starting point. Dividing each raw u32 timestamp
    // separately would insert a half-range discontinuity here.
    relay.downlink(&source_packet(
        202,
        source_start.wrapping_add(640),
        97,
        &payloads[1],
    ));
    relay.downlink(&source_packet(
        201,
        source_start.wrapping_add(320),
        97,
        &[0xf0, 0x7c],
    ));
    let second = receive(&relay.pbx);
    assert_eq!((second.seq, second.timestamp), (202, output_start + 320));
    assert_eq!(second.ssrc, SOURCE_SSRC);
    assert_eq!(second.payload.len(), 160);

    relay.downlink(&source_packet(
        203,
        source_start.wrapping_add(960),
        97,
        &payloads[2],
    ));
    let third = receive(&relay.pbx);
    assert_eq!((third.seq, third.timestamp), (203, output_start + 480));
    assert_no_packet(&relay.pbx, Duration::from_millis(60));
}

#[test]
fn uplink_keeps_its_generated_timeline_and_does_not_wait_for_source_sequence_gaps() {
    let relay = Relay::start(
        codec(NegotiatedCodec::AmrNb, 97),
        codec(NegotiatedCodec::Pcmu, 0),
        Duration::from_millis(500),
    );
    relay
        .carrier
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut received = Vec::new();
    for (seq, timestamp) in [(90, 200), (92, 9_000), (91, 100)] {
        relay
            .pbx
            .send(&source_packet(seq, timestamp, 0, &[0x55; 160]))
            .unwrap();
        received.push(receive(&relay.carrier));
    }
    for pair in received.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1));
        assert_eq!(pair[1].timestamp, pair[0].timestamp.wrapping_add(160));
        assert_eq!(pair[1].ssrc, pair[0].ssrc);
    }
    assert!(received.iter().all(|packet| packet.payload_type == 97));
    assert!(received
        .iter()
        .all(|packet| { amr_rtp::payload_to_frame(&packet.payload, AmrKind::Nb, true).is_some() }));
}
