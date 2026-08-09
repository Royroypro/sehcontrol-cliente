// RTP/H.264 packetization (RFC 6184) for Sehcontrol ScreenCam.
//
// This is intentionally minimal for the Fase 1 MVP (see docs/SCREENCAM_PLAN.md):
// single NAL unit packets for anything that fits the MTU, FU-A fragmentation
// (RFC 6184 §5.8) for anything larger. STAP-A aggregation is not implemented —
// harmless for correctness, just slightly less efficient for small NALs (SPS/PPS),
// which is an acceptable trade for how much simpler the payloader stays.

/// Splits an Annex-B H.264 bytestream (0x000001 or 0x00000001 start codes,
/// as emitted by the ffmpeg-based hardware encoders in libs/hwcodec) into its
/// individual NAL units, each *without* its start code.
pub fn split_annexb_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for (w, &begin) in starts.iter().enumerate() {
        let mut end = starts
            .get(w + 1)
            .map(|&next| next - 3)
            .unwrap_or(data.len());
        // A 4-byte start code (00 00 00 01) shows up here as one extra trailing
        // zero byte on the previous NAL — trim it regardless of which start
        // code length was used.
        while end > begin && data[end - 1] == 0 {
            end -= 1;
        }
        if end > begin {
            nals.push(&data[begin..end]);
        }
    }
    nals
}

/// NAL unit type of the first byte of a NAL (low 5 bits), see ITU-T H.264 Table 7-1.
pub fn nal_unit_type(nal: &[u8]) -> u8 {
    nal.first().copied().unwrap_or(0) & 0x1F
}

pub const NAL_TYPE_SPS: u8 = 7;
pub const NAL_TYPE_PPS: u8 = 8;
pub const NAL_TYPE_IDR: u8 = 5;

/// An IDR slice needs data after its one-byte NAL header. ScreenCam calls
/// this only for complete Annex-B access units returned by the encoder,
/// before any RTP fragmentation happens.
pub fn is_complete_idr_nal(nal: &[u8]) -> bool {
    nal.len() > 1 && nal_unit_type(nal) == NAL_TYPE_IDR
}

const RTP_VERSION_BYTE: u8 = 0x80; // V=2, P=0, X=0, CC=0
const RTP_PAYLOAD_TYPE_H264: u8 = 96; // dynamic payload type, negotiated via SDP
const FU_A_TYPE: u8 = 28;
pub const RTP_HEADER_LEN: usize = 12;

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
const NTP_UNIX_OFFSET_SECS: u64 = 2_208_988_800;

/// What the RTSP threads need to know about the live RTP stream without owning
/// the payloader, which lives in the capture loop: `RTP-Info` on PLAY needs the
/// next sequence number and the current RTP timestamp, and RTCP Sender Reports
/// need those plus the running packet/octet counters and the wall clock the
/// timestamp was sampled at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpStreamSnapshot {
    pub epoch: u64,
    pub ssrc: u32,
    /// Sequence number the *next* packet will carry, which is what a client
    /// that just issued PLAY is about to receive.
    pub next_seq: u16,
    pub timestamp_90k: u32,
    pub packet_count: u32,
    pub octet_count: u32,
    /// 64-bit NTP timestamp (32.32 fixed point) sampled when `timestamp_90k`
    /// was last published. Zero until the first access unit goes out.
    pub ntp: u64,
}

/// Written only by the capture loop, read by every RTSP connection thread and
/// by the RTCP sender. A mutex rather than atomics because the fields have to
/// move together: an SR built from a sequence number and a timestamp taken
/// from different access units would report a clock the client can't trust.
#[derive(Default)]
pub struct RtpStreamStats {
    inner: std::sync::Mutex<Option<RtpStreamSnapshot>>,
}

impl RtpStreamStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// Publishes the starting point of a new stream epoch, before any access
    /// unit has been packetized. Called by the capture loop so PLAY can always
    /// answer with an `RTP-Info` for the epoch the client just described,
    /// instead of omitting the header until the first frame happens to land.
    pub fn begin_epoch(&self, epoch: u64, ssrc: u32, next_seq: u16) {
        *self.inner.lock().unwrap() = Some(RtpStreamSnapshot {
            epoch,
            ssrc,
            next_seq,
            timestamp_90k: 0,
            packet_count: 0,
            octet_count: 0,
            ntp: 0,
        });
    }

    /// Records one packetized access unit. `packets` are the RTP packets as
    /// they go on the wire; the octet counter an SR reports excludes RTP
    /// headers (RFC 3550 §6.4.1), hence the per-packet subtraction.
    pub fn record_access_unit(
        &self,
        epoch: u64,
        ssrc: u32,
        next_seq: u16,
        timestamp_90k: u32,
        packets: &[Vec<u8>],
    ) {
        let octets: u32 = packets
            .iter()
            .map(|packet| packet.len().saturating_sub(RTP_HEADER_LEN) as u32)
            .fold(0u32, |acc, len| acc.wrapping_add(len));
        let mut guard = self.inner.lock().unwrap();
        // A stale epoch must never overwrite a newer one: the capture loop can
        // still be draining frames from the display it just left.
        let (packet_count, octet_count) = match guard.as_ref() {
            Some(previous) if previous.epoch == epoch => {
                (previous.packet_count, previous.octet_count)
            }
            Some(previous) if previous.epoch > epoch => return,
            _ => (0, 0),
        };
        *guard = Some(RtpStreamSnapshot {
            epoch,
            ssrc,
            next_seq,
            timestamp_90k,
            packet_count: packet_count.wrapping_add(packets.len() as u32),
            octet_count: octet_count.wrapping_add(octets),
            ntp: now_ntp(),
        });
    }

    /// `None` while nothing has been published for `epoch` — a client that
    /// described an older stream gets no `RTP-Info` and no SR rather than one
    /// describing a stream it isn't receiving.
    pub fn snapshot_for_epoch(&self, epoch: u64) -> Option<RtpStreamSnapshot> {
        self.inner
            .lock()
            .unwrap()
            .filter(|snapshot| snapshot.epoch == epoch)
    }
}

fn now_ntp() -> u64 {
    let since_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = since_unix.as_secs().wrapping_add(NTP_UNIX_OFFSET_SECS);
    // The fraction is the sub-second part scaled to 2^32, per RFC 3550 §4.
    let frac = ((since_unix.subsec_nanos() as u64) << 32) / 1_000_000_000;
    (secs << 32) | frac
}

/// Builds the RTCP compound packet a receiver expects periodically: a Sender
/// Report (PT=200) carrying the RTP↔wall-clock mapping, followed by the SDES
/// CNAME (PT=202) that RFC 3550 §6.5.1 makes mandatory in every compound
/// packet. Several NVR firmwares drop a session that never reports.
pub fn build_sender_report(snapshot: &RtpStreamSnapshot, cname: &str) -> Vec<u8> {
    let mut packet = Vec::with_capacity(64);

    // Sender Report: V=2, P=0, RC=0.
    packet.push(0x80);
    packet.push(200);
    // Length in 32-bit words minus one: header(1) + ssrc(1) + 5 sender-info
    // words = 7 words payload after the length field itself.
    packet.extend_from_slice(&6u16.to_be_bytes());
    packet.extend_from_slice(&snapshot.ssrc.to_be_bytes());
    packet.extend_from_slice(&snapshot.ntp.to_be_bytes());
    packet.extend_from_slice(&snapshot.timestamp_90k.to_be_bytes());
    packet.extend_from_slice(&snapshot.packet_count.to_be_bytes());
    packet.extend_from_slice(&snapshot.octet_count.to_be_bytes());

    // SDES with a single chunk carrying a single CNAME item.
    let cname = cname.as_bytes();
    let cname = &cname[..cname.len().min(255)];
    let mut chunk = Vec::with_capacity(8 + cname.len());
    chunk.extend_from_slice(&snapshot.ssrc.to_be_bytes());
    chunk.push(1); // CNAME
    chunk.push(cname.len() as u8);
    chunk.extend_from_slice(cname);
    chunk.push(0); // item list terminator
    while chunk.len() % 4 != 0 {
        chunk.push(0);
    }
    packet.push(0x81); // V=2, P=0, SC=1
    packet.push(202);
    packet.extend_from_slice(&((chunk.len() / 4) as u16).to_be_bytes());
    packet.extend_from_slice(&chunk);

    packet
}

/// Stateful RTP packetizer: owns the sequence number and SSRC for one stream.
pub struct H264Payloader {
    seq: u16,
    ssrc: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_byte_start_codes_and_multiple_nals() {
        let data = [
            0, 0, 1, 0x67, 0x64, 0, 0, 1, 0x68, 0xee, 0, 0, 1, 0x65, 0x88,
        ];

        assert_eq!(
            split_annexb_nals(&data),
            vec![&[0x67, 0x64][..], &[0x68, 0xee][..], &[0x65, 0x88][..]]
        );
    }

    #[test]
    fn splits_four_byte_start_codes() {
        let data = [0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 1, 0x65, 0x88];

        assert_eq!(
            split_annexb_nals(&data),
            vec![&[0x67, 0x64][..], &[0x65, 0x88][..]]
        );
    }

    #[test]
    fn incomplete_annexb_data_does_not_produce_an_idr() {
        for data in [&[][..], &[0][..], &[0, 0][..], &[0, 0, 1][..]] {
            assert!(split_annexb_nals(data).is_empty());
        }
        let header_only = split_annexb_nals(&[0, 0, 1, 0x65]);
        assert_eq!(header_only.len(), 1);
        assert!(!is_complete_idr_nal(header_only[0]));
    }

    #[test]
    fn only_complete_type_five_nals_are_idr() {
        assert!(is_complete_idr_nal(&[0x65, 0x88]));
        assert!(!is_complete_idr_nal(&[0x65]));
        assert!(!is_complete_idr_nal(&[0x67, 0x64]));
        assert!(!is_complete_idr_nal(&[0x68, 0xee]));
        assert!(!is_complete_idr_nal(&[0x66, 0x01])); // SEI
        assert!(!is_complete_idr_nal(&[0x69, 0x10])); // AUD
        assert!(!is_complete_idr_nal(&[0x61, 0x20])); // non-IDR slice
    }
}

impl H264Payloader {
    pub fn new() -> Self {
        Self {
            seq: hbb_common::rand::random(),
            ssrc: hbb_common::rand::random(),
        }
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Sequence number the next packetized RTP packet will carry — what
    /// `RTP-Info` has to advertise on PLAY.
    pub fn next_seq(&self) -> u16 {
        self.seq
    }

    /// Packetizes one encoded access unit (all the NAL units of a single
    /// frame) into RTP packets ready to send as-is. `timestamp_90k` is the
    /// RTP timestamp (90kHz clock, per RFC 6184 §8.2.1) shared by every
    /// packet of this access unit. `mtu` is the max RTP payload size
    /// (excluding the 12-byte RTP header).
    pub fn packetize(&mut self, nals: &[&[u8]], timestamp_90k: u32, mtu: usize) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        let last_idx = nals.len().saturating_sub(1);
        for (idx, nal) in nals.iter().copied().enumerate() {
            if nal.is_empty() {
                continue;
            }
            let is_last_nal = idx == last_idx;
            if nal.len() <= mtu {
                let mut pkt = self.rtp_header(timestamp_90k, is_last_nal);
                pkt.extend_from_slice(nal);
                packets.push(pkt);
            } else {
                self.fragment_fu_a(nal, timestamp_90k, mtu, is_last_nal, &mut packets);
            }
        }
        packets
    }

    fn fragment_fu_a(
        &mut self,
        nal: &[u8],
        timestamp_90k: u32,
        mtu: usize,
        is_last_nal: bool,
        out: &mut Vec<Vec<u8>>,
    ) {
        let header_byte = nal[0];
        let fu_indicator = (header_byte & 0xE0) | FU_A_TYPE; // keep forbidden_zero_bit + nal_ref_idc
        let nal_type = header_byte & 0x1F;
        let payload = &nal[1..];
        let max_fragment = mtu.saturating_sub(2).max(1); // 2 bytes of FU-A header
        let total = payload.len();
        let mut offset = 0usize;
        while offset < total {
            let end = (offset + max_fragment).min(total);
            let is_start = offset == 0;
            let is_end = end == total;
            let mut fu_header = nal_type;
            if is_start {
                fu_header |= 0x80;
            }
            if is_end {
                fu_header |= 0x40;
            }
            let marker = is_end && is_last_nal;
            let mut pkt = self.rtp_header(timestamp_90k, marker);
            pkt.push(fu_indicator);
            pkt.push(fu_header);
            pkt.extend_from_slice(&payload[offset..end]);
            out.push(pkt);
            offset = end;
        }
    }

    fn rtp_header(&mut self, timestamp: u32, marker: bool) -> Vec<u8> {
        let mut h = Vec::with_capacity(12);
        h.push(RTP_VERSION_BYTE);
        h.push(RTP_PAYLOAD_TYPE_H264 | if marker { 0x80 } else { 0 });
        h.push((self.seq >> 8) as u8);
        h.push((self.seq & 0xFF) as u8);
        self.seq = self.seq.wrapping_add(1);
        h.extend_from_slice(&timestamp.to_be_bytes());
        h.extend_from_slice(&self.ssrc.to_be_bytes());
        h
    }
}

#[cfg(test)]
mod rtcp_tests {
    use super::*;

    fn snapshot() -> RtpStreamSnapshot {
        RtpStreamSnapshot {
            epoch: 3,
            ssrc: 0xDEAD_BEEF,
            next_seq: 42,
            timestamp_90k: 900_000,
            packet_count: 7,
            octet_count: 1234,
            ntp: 0x0102_0304_0506_0708,
        }
    }

    #[test]
    fn sender_report_has_the_layout_rfc3550_specifies() {
        let packet = build_sender_report(&snapshot(), "screencam@sehcontrol");

        // Sender Report header.
        assert_eq!(packet[0], 0x80, "V=2, P=0, RC=0");
        assert_eq!(packet[1], 200, "PT=SR");
        assert_eq!(u16::from_be_bytes([packet[2], packet[3]]), 6);
        assert_eq!(
            u32::from_be_bytes(packet[4..8].try_into().unwrap()),
            0xDEAD_BEEF
        );
        assert_eq!(
            u64::from_be_bytes(packet[8..16].try_into().unwrap()),
            0x0102_0304_0506_0708
        );
        assert_eq!(
            u32::from_be_bytes(packet[16..20].try_into().unwrap()),
            900_000
        );
        assert_eq!(u32::from_be_bytes(packet[20..24].try_into().unwrap()), 7);
        assert_eq!(u32::from_be_bytes(packet[24..28].try_into().unwrap()), 1234);

        // SDES chunk follows immediately.
        assert_eq!(packet[28], 0x81, "V=2, SC=1");
        assert_eq!(packet[29], 202, "PT=SDES");
        let sdes_words = u16::from_be_bytes([packet[30], packet[31]]) as usize;
        assert_eq!(
            packet.len(),
            28 + 4 + sdes_words * 4,
            "declared length covers the rest"
        );
        assert_eq!(
            u32::from_be_bytes(packet[32..36].try_into().unwrap()),
            0xDEAD_BEEF
        );
        assert_eq!(packet[36], 1, "CNAME item");
        let cname_len = packet[37] as usize;
        assert_eq!(&packet[38..38 + cname_len], b"screencam@sehcontrol");
        assert_eq!(packet[38 + cname_len], 0, "item list terminator");
        assert_eq!(packet.len() % 4, 0, "compound packet is 32-bit aligned");
    }

    #[test]
    fn every_cname_length_keeps_the_packet_aligned() {
        for len in 0..40usize {
            let cname = "x".repeat(len);
            let packet = build_sender_report(&snapshot(), &cname);
            assert_eq!(packet.len() % 4, 0, "cname len {len}");
            let sdes_words = u16::from_be_bytes([packet[30], packet[31]]) as usize;
            assert_eq!(packet.len(), 32 + sdes_words * 4, "cname len {len}");
        }
    }

    #[test]
    fn octet_count_excludes_rtp_headers_and_accumulates_within_an_epoch() {
        let stats = RtpStreamStats::new();
        stats.begin_epoch(1, 9, 100);
        let packet = vec![0u8; RTP_HEADER_LEN + 50];

        stats.record_access_unit(1, 9, 101, 3000, &[packet.clone()]);
        stats.record_access_unit(1, 9, 102, 6000, &[packet.clone(), packet.clone()]);

        let snapshot = stats.snapshot_for_epoch(1).expect("published");
        assert_eq!(snapshot.packet_count, 3);
        assert_eq!(snapshot.octet_count, 150);
        assert_eq!(snapshot.next_seq, 102);
        assert_eq!(snapshot.timestamp_90k, 6000);
        assert!(snapshot.ntp > 0);
    }

    #[test]
    fn a_new_epoch_restarts_the_counters_and_a_stale_one_is_ignored() {
        let stats = RtpStreamStats::new();
        let packet = vec![0u8; RTP_HEADER_LEN + 10];
        stats.record_access_unit(1, 9, 5, 3000, &[packet.clone()]);
        stats.record_access_unit(2, 9, 6, 100, &[packet.clone()]);
        // A frame still draining from the display we just left.
        stats.record_access_unit(1, 9, 7, 9000, &[packet.clone()]);

        assert_eq!(stats.snapshot_for_epoch(1), None, "old epoch is gone");
        let snapshot = stats.snapshot_for_epoch(2).expect("current epoch");
        assert_eq!(
            snapshot.packet_count, 1,
            "counters restarted, stale unit ignored"
        );
        assert_eq!(snapshot.next_seq, 6);
    }

    #[test]
    fn nothing_is_published_before_an_epoch_begins() {
        assert_eq!(RtpStreamStats::new().snapshot_for_epoch(0), None);
    }

    #[test]
    fn begin_epoch_publishes_a_zero_clock_the_reporter_can_recognise() {
        let stats = RtpStreamStats::new();
        stats.begin_epoch(4, 77, 1000);
        let snapshot = stats.snapshot_for_epoch(4).expect("published");
        assert_eq!(
            snapshot.ntp, 0,
            "reporter skips a session with no clock yet"
        );
        assert_eq!(snapshot.next_seq, 1000, "RTP-Info can answer immediately");
    }
}
