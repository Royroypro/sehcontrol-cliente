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
        let mut end = starts.get(w + 1).map(|&next| next - 3).unwrap_or(data.len());
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

const RTP_VERSION_BYTE: u8 = 0x80; // V=2, P=0, X=0, CC=0
const RTP_PAYLOAD_TYPE_H264: u8 = 96; // dynamic payload type, negotiated via SDP
const FU_A_TYPE: u8 = 28;

/// Stateful RTP packetizer: owns the sequence number and SSRC for one stream.
pub struct H264Payloader {
    seq: u16,
    ssrc: u32,
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
