// Minimal MPEG-TS muxer for the ScreenCam SRT preview.
//
// Takes one complete H.264 Annex-B access unit at a time and returns the
// transport stream packets that carry it: PAT, PMT and a single PES per
// access unit, at 188 bytes per packet. Scope is deliberately narrow — video
// only, one program, one elementary stream, no PCR/PTS interpolation, no
// remuxing of what the encoder produced. The Annex-B payload crosses this
// module untouched.
//
// Everything is pure and deterministic: no clock, no I/O, no globals, no
// async. The muxer keeps exactly the state MPEG-TS requires between access
// units (continuity counters, when PSI was last emitted, and whether a
// discontinuity still has to be announced) and `reset()` returns it to its
// initial state.
//
// `mux_access_unit` is transactional: it builds against a copy of that state
// and only commits once the whole access unit has been packetized, so a
// failed call leaves nothing behind for the next one to trip over.
//
// References are to ISO/IEC 13818-1 (MPEG-2 systems).

/// Transport stream packet size, ISO/IEC 13818-1 §2.4.3.2. Every buffer this
/// module returns is a whole number of these.
pub(crate) const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
/// Bytes left for the adaptation field plus payload, after the 4-byte header.
const TS_BODY_SIZE: usize = TS_PACKET_SIZE - 4;

const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const PROGRAM_NUMBER: u16 = 1;
const PES_STREAM_ID_VIDEO: u8 = 0xE0;
const STREAM_TYPE_H264: u8 = 0x1B;

/// PAT/PMT are repeated at least this often (measured on the access units'
/// own presentation timestamps, never on the system clock, so the output for
/// a given sequence of access units is reproducible).
const PSI_REPEAT_INTERVAL_MS: i64 = 500;

/// 33-bit wrap of the 90 kHz system time base, ISO/IEC 13818-1 §2.4.3.7.
const PTS_MODULUS: i128 = 1 << 33;

/// 4 bytes of start code + stream id, 2 of packet length, 3 of optional
/// header, 5 of PTS.
const PES_HEADER_LEN: usize = 14;

/// Smallest adaptation field that still carries a PCR: the length byte, the
/// flags byte and the 6 PCR bytes.
const PCR_ADAPTATION_LEN: usize = 8;

/// Payload the first packet of an access unit has left once its mandatory
/// PCR-bearing adaptation field is accounted for.
const FIRST_PACKET_CAPACITY: usize = TS_BODY_SIZE - PCR_ADAPTATION_LEN;

// Adaptation field flags, ISO/IEC 13818-1 §2.4.3.4.
const AF_DISCONTINUITY: u8 = 0x80;
const AF_RANDOM_ACCESS: u8 = 0x40;
const AF_PCR: u8 = 0x10;

/// Reasons an access unit cannot be muxed. Deliberately free of any detail
/// taken from the frame itself — these codes are meant to be safe to log and
/// to forward to the panel as-is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MpegTsError {
    /// `pts_ms` was below zero; the 90 kHz time base is unsigned.
    NegativePts,
    /// The access unit had no bytes at all.
    EmptyAccessUnit,
    /// The access unit did not begin with an Annex-B start code.
    InvalidAnnexB,
    /// The access unit could not be packetized: either it is so large that
    /// the output size does not fit a `usize`, or a packet did not come out
    /// at exactly [`TS_PACKET_SIZE`]. The muxer fails closed rather than emit
    /// a malformed stream, and its state is left untouched.
    InternalPacketization,
}

impl std::fmt::Display for MpegTsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NegativePts => "negative presentation timestamp",
            Self::EmptyAccessUnit => "empty access unit",
            Self::InvalidAnnexB => "access unit is not Annex-B",
            Self::InternalPacketization => "internal transport packetization error",
        })
    }
}

impl std::error::Error for MpegTsError {}

/// Everything that survives between access units, kept in its own `Copy`
/// struct so `mux_access_unit` can work on a snapshot and commit it in one
/// assignment. Nothing here may be mutated outside that commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MuxerState {
    pat_continuity: u8,
    pmt_continuity: u8,
    video_continuity: u8,
    /// `None` until PSI has been emitted at least once, which is also what
    /// forces PAT/PMT ahead of the very first access unit after `reset()`.
    last_psi_pts_ms: Option<i64>,
    /// Set by `reset()`, cleared once the next access unit has announced the
    /// discontinuity. A brand new muxer has nothing to announce.
    discontinuity_pending: bool,
}

impl MuxerState {
    const fn new() -> Self {
        Self {
            pat_continuity: 0,
            pmt_continuity: 0,
            video_continuity: 0,
            last_psi_pts_ms: None,
            discontinuity_pending: false,
        }
    }

    /// PSI goes out ahead of the first access unit, ahead of every keyframe
    /// (so a decoder joining at a random access point has the tables it needs
    /// immediately) and whenever the repeat interval has elapsed. A keyframe
    /// that also crosses the interval still only emits one copy — this is a
    /// single decision, not two.
    fn psi_due(&self, pts_ms: i64, keyframe: bool) -> bool {
        match self.last_psi_pts_ms {
            None => true,
            // saturating: a backwards PTS must not wrap into "due".
            Some(last) => keyframe || pts_ms.saturating_sub(last) >= PSI_REPEAT_INTERVAL_MS,
        }
    }

    /// Returns the continuity counter to stamp on the next packet of `pid`
    /// and advances it, wrapping 15 → 0 (ISO/IEC 13818-1 §2.4.3.3). Only
    /// called for packets that actually carry payload, which is the condition
    /// the standard attaches the increment to.
    fn next_continuity(&mut self, pid: Pid) -> u8 {
        let counter = match pid {
            Pid::Pat => &mut self.pat_continuity,
            Pid::Pmt => &mut self.pmt_continuity,
            Pid::Video => &mut self.video_continuity,
        };
        let current = *counter;
        *counter = (current + 1) & 0x0F;
        current
    }
}

/// Muxes H.264 access units into MPEG-TS. One instance per preview session:
/// the continuity counters and the PSI schedule are per-stream state, so a
/// new session must start from `new()`, and a stream epoch change within a
/// session from `reset()`.
pub(crate) struct MpegTsMuxer {
    state: MuxerState,
    /// Test-only seam for [`MpegTsError::InternalPacketization`], which the
    /// algorithm cannot reach on its own. Never compiled into production and
    /// never read outside `write_video_packets`.
    #[cfg(test)]
    fail_next_packetization: bool,
}

impl Default for MpegTsMuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl MpegTsMuxer {
    pub(crate) fn new() -> Self {
        Self {
            state: MuxerState::new(),
            #[cfg(test)]
            fail_next_packetization: false,
        }
    }

    /// Returns the muxer to its initial state — continuity counters back to
    /// zero, PSI due again — and arms the discontinuity announcement.
    ///
    /// Used when the stream epoch changes (a new capture, a new resolution).
    /// Restarting the counters without saying so reads to a demuxer as lost
    /// packets: ffmpeg reports "Packet corrupt" at exactly that point. The
    /// next access unit therefore sets `discontinuity_indicator`, which is
    /// how the standard spells "the counters and the clock restart here, this
    /// is not packet loss".
    pub(crate) fn reset(&mut self) {
        self.state = MuxerState::new();
        self.state.discontinuity_pending = true;
    }

    /// Muxes one complete access unit. `annexb` must be the encoder's output
    /// for a single access unit, start codes included; it is copied into the
    /// PES payload verbatim and no reference to it is retained.
    ///
    /// The returned buffer is always a whole number of [`TS_PACKET_SIZE`]
    /// packets. On `Err` the muxer's observable state — every continuity
    /// counter, the PSI schedule and the pending discontinuity — is exactly
    /// what it was before the call.
    pub(crate) fn mux_access_unit(
        &mut self,
        pts_ms: i64,
        keyframe: bool,
        annexb: &[u8],
    ) -> Result<Vec<u8>, MpegTsError> {
        if pts_ms < 0 {
            return Err(MpegTsError::NegativePts);
        }
        if annexb.is_empty() {
            return Err(MpegTsError::EmptyAccessUnit);
        }
        if !starts_with_start_code(annexb) {
            return Err(MpegTsError::InvalidAnnexB);
        }
        // i128 keeps the ×90 from ever overflowing for any i64 input, and
        // rem_euclid keeps the result inside the 33-bit field.
        let pts_90khz = ((pts_ms as i128) * 90).rem_euclid(PTS_MODULUS) as u64;

        // Every mutation below lands on this copy; `self` is only touched by
        // the single commit at the end, so any early return leaves the muxer
        // exactly as the caller found it.
        let mut state = self.state;
        let emit_psi = state.psi_due(pts_ms, keyframe);
        let pes_len = PES_HEADER_LEN
            .checked_add(annexb.len())
            .ok_or(MpegTsError::InternalPacketization)?;
        let mut out = Vec::with_capacity(output_capacity(pes_len, emit_psi)?);

        if emit_psi {
            let continuity = state.next_continuity(Pid::Pat);
            write_section_packet(&mut out, PAT_PID, continuity, &pat_section())?;
            let continuity = state.next_continuity(Pid::Pmt);
            write_section_packet(&mut out, PMT_PID, continuity, &pmt_section())?;
            state.last_psi_pts_ms = Some(pts_ms);
        }
        self.write_video_packets(&mut state, &mut out, pts_90khz, keyframe, pes_len, annexb)?;

        if out.is_empty() || out.len() % TS_PACKET_SIZE != 0 {
            return Err(MpegTsError::InternalPacketization);
        }
        // The discontinuity has now been announced on the wire.
        state.discontinuity_pending = false;
        self.state = state;
        Ok(out)
    }

    fn write_video_packets(
        &self,
        state: &mut MuxerState,
        out: &mut Vec<u8>,
        pts_90khz: u64,
        keyframe: bool,
        pes_len: usize,
        annexb: &[u8],
    ) -> Result<(), MpegTsError> {
        let mut pes = Vec::with_capacity(pes_len);
        pes.extend_from_slice(&[0x00, 0x00, 0x01, PES_STREAM_ID_VIDEO]);
        // PES_packet_length 0 = unbounded, the only legal choice for video
        // whose length isn't known before packetization (§2.4.3.7).
        pes.extend_from_slice(&[0x00, 0x00]);
        pes.push(0x80); // '10' marker, not scrambled, not aligned, not a copy
        pes.push(0x80); // PTS_DTS_flags = '10' — PTS only, no DTS
        pes.push(0x05); // PES_header_data_length: just the 5 PTS bytes
        pes.extend_from_slice(&encode_timestamp(pts_90khz));
        pes.extend_from_slice(annexb);

        let mut offset = 0usize;
        let mut first = true;
        while offset < pes.len() {
            #[cfg(test)]
            if self.fail_next_packetization && !first {
                // Deliberately mid-access-unit: PSI counters, the PSI
                // schedule and at least one video counter have already moved
                // on the snapshot, so the caller's rollback is what has to
                // put them back.
                return Err(MpegTsError::InternalPacketization);
            }
            let remaining = pes.len() - offset;
            let (adaptation, take) = if first {
                // The first packet always carries the PCR and the random
                // access indicator, so its adaptation field is never absent;
                // whatever the PES doesn't fill becomes adaptation stuffing.
                //
                // The discontinuity indicator rides here too, on the video
                // PID rather than on PAT/PMT: this is the PID that carries
                // the PCR, and §2.4.3.4 makes the flag mean both "the
                // continuity counter restarts" and "the clock restarts" for
                // exactly that stream. PAT/PMT restart their counters as
                // well, but they are re-sent as complete sections, so a
                // demuxer reacquires them without needing the announcement.
                let take = remaining.min(FIRST_PACKET_CAPACITY);
                let mut field = Vec::with_capacity(7 + (FIRST_PACKET_CAPACITY - take));
                let mut flags = AF_PCR;
                if keyframe {
                    flags |= AF_RANDOM_ACCESS;
                }
                if state.discontinuity_pending {
                    flags |= AF_DISCONTINUITY;
                }
                field.push(flags);
                field.extend_from_slice(&encode_pcr(pts_90khz));
                field.resize(7 + (FIRST_PACKET_CAPACITY - take), 0xFF);
                (Some(field), take)
            } else if remaining >= TS_BODY_SIZE {
                (None, TS_BODY_SIZE)
            } else {
                // Short tail: pad with an adaptation field rather than
                // splitting the access unit across a padding packet.
                let stuffing = TS_BODY_SIZE - remaining;
                let mut field = Vec::with_capacity(stuffing.saturating_sub(1));
                if stuffing >= 2 {
                    field.push(0x00); // no flags set
                    field.resize(stuffing - 1, 0xFF);
                }
                // stuffing == 1 leaves an empty field, i.e. a lone
                // adaptation_field_length of 0 — the standard's one-byte pad.
                (Some(field), remaining)
            };

            let end = offset
                .checked_add(take)
                .filter(|end| *end <= pes.len())
                .ok_or(MpegTsError::InternalPacketization)?;
            let continuity = state.next_continuity(Pid::Video);
            write_ts_packet(
                out,
                VIDEO_PID,
                first,
                continuity,
                adaptation.as_deref(),
                &pes[offset..end],
            )?;
            offset = end;
            first = false;
        }
        Ok(())
    }

    #[cfg(test)]
    fn snapshot(&self) -> MuxerState {
        self.state
    }

    #[cfg(test)]
    fn continuity_counters(&self) -> (u8, u8, u8) {
        (
            self.state.pat_continuity,
            self.state.pmt_continuity,
            self.state.video_continuity,
        )
    }
}

enum Pid {
    Pat,
    Pmt,
    Video,
}

/// Upper bound on the bytes one access unit will occupy, so the output vector
/// is allocated once. Overflow is impossible for any real slice, but it is
/// still checked rather than assumed — a wrong guess here would only cost a
/// reallocation, a wrap would corrupt the allocation.
fn output_capacity(pes_len: usize, emit_psi: bool) -> Result<usize, MpegTsError> {
    let video_packets = if pes_len <= FIRST_PACKET_CAPACITY {
        1
    } else {
        1 + (pes_len - FIRST_PACKET_CAPACITY).div_ceil(TS_BODY_SIZE)
    };
    video_packets
        .checked_add(if emit_psi { 2 } else { 0 })
        .and_then(|packets| packets.checked_mul(TS_PACKET_SIZE))
        .ok_or(MpegTsError::InternalPacketization)
}

/// Accepts an access unit that begins with an Annex-B start code, allowing
/// the `leading_zero_8bits` that H.264 Annex B permits in front of it
/// (`00 00 00 00 01 …` is as legal as `00 00 01 …`), and requires at least
/// one byte of NAL behind it — a buffer that is nothing but zeros and a start
/// code is not an access unit.
///
/// The leading zeros are only *tolerated*, never removed: the PES carries the
/// caller's buffer verbatim. No further parsing either — the encoder owns NAL
/// structure, this only rejects input that evidently isn't Annex-B at all.
fn starts_with_start_code(data: &[u8]) -> bool {
    // Bounded by the buffer length, so this cannot run away.
    let mut index = 0usize;
    while index < data.len() && data[index] == 0x00 {
        index += 1;
    }
    // The scan stopped on the first non-zero byte. For the prefix to be a
    // start code that byte has to be the closing 0x01, with at least the two
    // zeros of `00 00 01` in front of it.
    if index < 2 || index >= data.len() || data[index] != 0x01 {
        return false;
    }
    // At least one NAL byte after the start code.
    index + 1 < data.len()
}

/// The 5-byte '0010'-prefixed PTS field of §2.4.3.7, with its three marker
/// bits. Only the low 33 bits of `value` are encoded.
fn encode_timestamp(value: u64) -> [u8; 5] {
    [
        0x21 | (((value >> 30) & 0x07) as u8) << 1,
        ((value >> 22) & 0xFF) as u8,
        0x01 | (((value >> 15) & 0x7F) as u8) << 1,
        ((value >> 7) & 0xFF) as u8,
        0x01 | ((value & 0x7F) as u8) << 1,
    ]
}

/// The 6-byte program_clock_reference of §2.4.3.5: a 33-bit base, 6 reserved
/// bits set to 1 and a 9-bit extension. The extension is always zero here —
/// the preview's clock resolution is the encoder's millisecond PTS, so the
/// 27 MHz refinement carries no information we actually have.
fn encode_pcr(base_90khz: u64) -> [u8; 6] {
    let base = base_90khz & 0x1_FFFF_FFFF;
    [
        ((base >> 25) & 0xFF) as u8,
        ((base >> 17) & 0xFF) as u8,
        ((base >> 9) & 0xFF) as u8,
        ((base >> 1) & 0xFF) as u8,
        (((base & 0x01) as u8) << 7) | 0x7E,
        0x00,
    ]
}

/// CRC-32/MPEG-2 (poly 0x04C11DB7, init 0xFFFFFFFF, no reflection, no final
/// xor) — the PSI section CRC of ISO/IEC 13818-1 Annex B. Deliberately not
/// CRC-32/IEEE, which is reflected and would be rejected by every demuxer.
fn crc32_mpeg2(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= (byte as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Program Association Table: one program, pointing at [`PMT_PID`].
fn pat_section() -> Vec<u8> {
    let mut section = Vec::with_capacity(16);
    // table_id, then section_syntax_indicator '1', '0', reserved '11' and a
    // section_length of 13 (the 16 bytes below, less this 3-byte prologue).
    section.push(0x00);
    section.extend_from_slice(&[0xB0, 0x0D]);
    section.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes()); // transport_stream_id
    section.push(0xC1); // reserved '11', version 0, current_next_indicator 1
    section.push(0x00); // section_number
    section.push(0x00); // last_section_number
    section.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
    section.extend_from_slice(&pid_with_reserved(PMT_PID, 0xE0));
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// Program Map Table: program 1, PCR on the video PID, one H.264 stream and
/// no descriptors at either level.
fn pmt_section() -> Vec<u8> {
    let mut section = Vec::with_capacity(21);
    // table_id, then section_syntax_indicator '1', '0', reserved '11' and a
    // section_length of 18 (the 21 bytes below, less this 3-byte prologue).
    section.push(0x02);
    section.extend_from_slice(&[0xB0, 0x12]);
    section.extend_from_slice(&PROGRAM_NUMBER.to_be_bytes());
    section.push(0xC1); // reserved '11', version 0, current_next_indicator 1
    section.push(0x00); // section_number
    section.push(0x00); // last_section_number
    section.extend_from_slice(&pid_with_reserved(VIDEO_PID, 0xE0)); // PCR_PID
    section.extend_from_slice(&[0xF0, 0x00]); // program_info_length 0
    section.push(STREAM_TYPE_H264);
    section.extend_from_slice(&pid_with_reserved(VIDEO_PID, 0xE0)); // elementary_PID
    section.extend_from_slice(&[0xF0, 0x00]); // ES_info_length 0
    let crc = crc32_mpeg2(&section);
    section.extend_from_slice(&crc.to_be_bytes());
    section
}

/// A 13-bit PID preceded by the reserved bits the surrounding field requires.
fn pid_with_reserved(pid: u16, reserved: u8) -> [u8; 2] {
    [reserved | ((pid >> 8) as u8 & 0x1F), (pid & 0xFF) as u8]
}

/// Wraps one PSI section in a single TS packet: pointer_field, section, then
/// 0xFF to the end of the packet. Both tables here are far smaller than one
/// packet, so no section ever needs to be continued.
fn write_section_packet(
    out: &mut Vec<u8>,
    pid: u16,
    continuity: u8,
    section: &[u8],
) -> Result<(), MpegTsError> {
    if section.len() + 1 > TS_BODY_SIZE {
        return Err(MpegTsError::InternalPacketization);
    }
    let mut payload = Vec::with_capacity(TS_BODY_SIZE);
    payload.push(0x00); // pointer_field: section starts immediately
    payload.extend_from_slice(section);
    payload.resize(TS_BODY_SIZE, 0xFF);
    write_ts_packet(out, pid, true, continuity, None, &payload)
}

/// Appends exactly one 188-byte packet. `adaptation` is the field's contents
/// *after* its length byte: `None` for no adaptation field at all, `Some(&[])`
/// for the one-byte field that pads a packet by exactly one byte.
fn write_ts_packet(
    out: &mut Vec<u8>,
    pid: u16,
    payload_unit_start: bool,
    continuity: u8,
    adaptation: Option<&[u8]>,
    payload: &[u8],
) -> Result<(), MpegTsError> {
    let adaptation_len = adaptation.map_or(0, |field| 1 + field.len());
    if 4 + adaptation_len + payload.len() != TS_PACKET_SIZE || payload.is_empty() {
        return Err(MpegTsError::InternalPacketization);
    }
    let adaptation_field_control: u8 = if adaptation.is_some() { 0x30 } else { 0x10 };

    out.push(TS_SYNC_BYTE);
    // transport_error_indicator 0, PUSI, transport_priority 0, PID high bits.
    out.push(if payload_unit_start { 0x40 } else { 0x00 } | ((pid >> 8) as u8 & 0x1F));
    out.push((pid & 0xFF) as u8);
    // transport_scrambling_control '00', adaptation_field_control, continuity.
    out.push(adaptation_field_control | (continuity & 0x0F));
    if let Some(field) = adaptation {
        // Bounded by the length check above: the field can never exceed 182.
        out.push(field.len() as u8);
        out.extend_from_slice(field);
    }
    out.extend_from_slice(payload);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact PAT this muxer must emit, CRC included. Computed outside
    /// this crate (independent CRC-32/MPEG-2 implementation) so the tests
    /// below never check `crc32_mpeg2` against itself.
    const EXPECTED_PAT_SECTION: [u8; 16] = [
        0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x2A, 0xB1, 0x04,
        0xB2,
    ];
    const EXPECTED_PAT_CRC: u32 = 0x2AB1_04B2;
    /// Likewise for the PMT.
    const EXPECTED_PMT_SECTION: [u8; 21] = [
        0x02, 0xB0, 0x12, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00, 0x1B, 0xE1, 0x00,
        0xF0, 0x00, 0x15, 0xBD, 0x4D, 0x56,
    ];
    const EXPECTED_PMT_CRC: u32 = 0x15BD_4D56;

    /// A parsed TS packet. The tests decode the muxer's output through this
    /// rather than matching byte patterns, so a packet that happens to
    /// contain the right bytes in the wrong structure still fails.
    #[derive(Debug)]
    struct ParsedPacket {
        pid: u16,
        payload_unit_start: bool,
        continuity: u8,
        adaptation: Option<Vec<u8>>,
        payload: Vec<u8>,
    }

    impl ParsedPacket {
        fn flag(&self, mask: u8) -> bool {
            self.adaptation
                .as_ref()
                .and_then(|field| field.first())
                .is_some_and(|flags| flags & mask != 0)
        }

        fn random_access(&self) -> bool {
            self.flag(AF_RANDOM_ACCESS)
        }

        fn discontinuity(&self) -> bool {
            self.flag(AF_DISCONTINUITY)
        }

        fn pcr(&self) -> Option<u64> {
            let field = self.adaptation.as_ref()?;
            let flags = *field.first()?;
            if flags & AF_PCR == 0 || field.len() < 7 {
                return None;
            }
            let bytes = &field[1..7];
            Some(
                ((bytes[0] as u64) << 25)
                    | ((bytes[1] as u64) << 17)
                    | ((bytes[2] as u64) << 9)
                    | ((bytes[3] as u64) << 1)
                    | ((bytes[4] as u64) >> 7),
            )
        }

        /// The PSI section this packet carries, honouring the pointer_field.
        fn section(&self) -> Vec<u8> {
            assert!(self.payload_unit_start, "section packet must set PUSI");
            let pointer = self.payload[0] as usize;
            let start = 1 + pointer;
            let length = (((self.payload[start + 1] as usize) & 0x0F) << 8)
                | self.payload[start + 2] as usize;
            self.payload[start..start + 3 + length].to_vec()
        }
    }

    fn parse_packets(data: &[u8]) -> Vec<ParsedPacket> {
        assert_eq!(data.len() % TS_PACKET_SIZE, 0, "not a whole packet count");
        data.chunks(TS_PACKET_SIZE)
            .map(|packet| {
                assert_eq!(packet[0], TS_SYNC_BYTE);
                assert_eq!(packet[1] & 0x80, 0, "transport_error_indicator set");
                assert_eq!(packet[3] & 0xC0, 0, "transport_scrambling_control set");
                let control = (packet[3] >> 4) & 0x03;
                assert_ne!(control, 0, "reserved adaptation_field_control");
                let mut body = 4;
                let adaptation = if control & 0x02 != 0 {
                    let length = packet[4] as usize;
                    body = 5 + length;
                    assert!(body <= TS_PACKET_SIZE, "adaptation field overruns packet");
                    Some(packet[5..5 + length].to_vec())
                } else {
                    None
                };
                let payload = if control & 0x01 != 0 {
                    packet[body..].to_vec()
                } else {
                    Vec::new()
                };
                ParsedPacket {
                    pid: (((packet[1] & 0x1F) as u16) << 8) | packet[2] as u16,
                    payload_unit_start: packet[1] & 0x40 != 0,
                    continuity: packet[3] & 0x0F,
                    adaptation,
                    payload,
                }
            })
            .collect()
    }

    fn of_pid(packets: &[ParsedPacket], pid: u16) -> Vec<&ParsedPacket> {
        packets.iter().filter(|packet| packet.pid == pid).collect()
    }

    /// Reassembles the PES from every video packet and strips its header,
    /// returning the Annex-B bytes a demuxer would hand the decoder. Only
    /// valid for a buffer holding a single access unit.
    fn reconstruct_annexb(packets: &[ParsedPacket]) -> Vec<u8> {
        let video = of_pid(packets, VIDEO_PID);
        assert!(video.first().is_some_and(|p| p.payload_unit_start));
        assert!(
            video[1..].iter().all(|p| !p.payload_unit_start),
            "helper only handles one PES"
        );
        let mut pes = Vec::new();
        for packet in video {
            pes.extend_from_slice(&packet.payload);
        }
        assert_eq!(&pes[..3], &[0x00, 0x00, 0x01]);
        assert_eq!(pes[3], PES_STREAM_ID_VIDEO);
        let header_data_length = pes[8] as usize;
        pes[9 + header_data_length..].to_vec()
    }

    fn decode_pts(packets: &[ParsedPacket]) -> u64 {
        let video = of_pid(packets, VIDEO_PID);
        let payload = &video[0].payload;
        let bytes = &payload[9..14];
        (((bytes[0] as u64) & 0x0E) << 29)
            | ((bytes[1] as u64) << 22)
            | (((bytes[2] as u64) & 0xFE) << 14)
            | ((bytes[3] as u64) << 7)
            | ((bytes[4] as u64) >> 1)
    }

    /// An access unit of `payload_len` bytes *including* its start code.
    fn access_unit(payload_len: usize) -> Vec<u8> {
        let mut unit = vec![0x00, 0x00, 0x00, 0x01, 0x65];
        assert!(payload_len >= unit.len());
        unit.resize(payload_len, 0xA5);
        unit
    }

    fn mux(muxer: &mut MpegTsMuxer, pts_ms: i64, keyframe: bool, len: usize) -> Vec<u8> {
        muxer
            .mux_access_unit(pts_ms, keyframe, &access_unit(len))
            .expect("access unit should mux")
    }

    /// The first video packet of a freshly muxed access unit.
    fn first_video(out: &[u8]) -> ParsedPacket {
        let packets = parse_packets(out);
        let index = packets
            .iter()
            .position(|packet| packet.pid == VIDEO_PID)
            .expect("an access unit always produces video packets");
        parse_packets(out).swap_remove(index)
    }

    // --- packet framing -------------------------------------------------

    #[test]
    fn every_packet_is_188_bytes_and_starts_with_the_sync_byte() {
        let mut muxer = MpegTsMuxer::new();
        let out = mux(&mut muxer, 0, true, 4000);
        assert_eq!(out.len() % TS_PACKET_SIZE, 0);
        for packet in out.chunks(TS_PACKET_SIZE) {
            assert_eq!(packet.len(), TS_PACKET_SIZE);
            assert_eq!(packet[0], TS_SYNC_BYTE);
        }
    }

    #[test]
    fn output_length_is_a_multiple_of_the_packet_size() {
        let mut muxer = MpegTsMuxer::new();
        for len in [5, 100, 176, 500, 4096] {
            assert_eq!(mux(&mut muxer, 0, false, len).len() % TS_PACKET_SIZE, 0);
        }
    }

    // --- PAT ------------------------------------------------------------

    #[test]
    fn pat_uses_pid_zero() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        assert_eq!(packets[0].pid, PAT_PID);
        assert!(packets[0].payload_unit_start);
        assert_eq!(packets[0].payload[0], 0x00, "pointer_field must be 0");
    }

    #[test]
    fn pat_announces_the_pmt_pid() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        let section = packets[0].section();
        assert_eq!(section[0], 0x00, "table_id");
        assert_eq!(section[5] & 0x01, 0x01, "current_next_indicator");
        assert_eq!(section[6], 0x00, "section_number");
        assert_eq!(section[7], 0x00, "last_section_number");
        let program = u16::from_be_bytes([section[8], section[9]]);
        let pmt_pid = (((section[10] & 0x1F) as u16) << 8) | section[11] as u16;
        assert_eq!(program, PROGRAM_NUMBER);
        assert_eq!(pmt_pid, PMT_PID);
    }

    #[test]
    fn pat_matches_the_precomputed_section_and_crc() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        let section = packets[0].section();
        assert_eq!(section, EXPECTED_PAT_SECTION, "PAT bytes drifted");
        let emitted = u32::from_be_bytes([section[12], section[13], section[14], section[15]]);
        assert_eq!(emitted, EXPECTED_PAT_CRC);
    }

    // --- PMT ------------------------------------------------------------

    #[test]
    fn pmt_uses_its_own_pid() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        assert_eq!(packets[1].pid, PMT_PID);
        assert!(packets[1].payload_unit_start);
        assert_eq!(packets[1].payload[0], 0x00, "pointer_field must be 0");
    }

    #[test]
    fn pmt_declares_h264_on_the_video_pid_with_the_video_pcr() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        let section = packets[1].section();
        assert_eq!(section[0], 0x02, "table_id");
        assert_eq!(u16::from_be_bytes([section[3], section[4]]), PROGRAM_NUMBER);
        let pcr_pid = (((section[8] & 0x1F) as u16) << 8) | section[9] as u16;
        assert_eq!(pcr_pid, VIDEO_PID);
        let program_info_length = (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
        assert_eq!(program_info_length, 0);
        assert_eq!(section[12], STREAM_TYPE_H264);
        let elementary_pid = (((section[13] & 0x1F) as u16) << 8) | section[14] as u16;
        assert_eq!(elementary_pid, VIDEO_PID);
        let es_info_length = (((section[15] as usize) & 0x0F) << 8) | section[16] as usize;
        assert_eq!(es_info_length, 0, "no descriptors expected");
    }

    #[test]
    fn pmt_matches_the_precomputed_section_and_crc() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        let section = packets[1].section();
        assert_eq!(section, EXPECTED_PMT_SECTION, "PMT bytes drifted");
        let emitted = u32::from_be_bytes([section[17], section[18], section[19], section[20]]);
        assert_eq!(emitted, EXPECTED_PMT_CRC);
    }

    #[test]
    fn psi_sections_carry_their_crc_residue() {
        // Supporting evidence only — the literals above are the real check.
        // Running CRC-32/MPEG-2 over a section *including* its CRC yields 0.
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        assert_eq!(crc32_mpeg2(&packets[0].section()), 0);
        assert_eq!(crc32_mpeg2(&packets[1].section()), 0);
    }

    // --- PSI schedule ---------------------------------------------------

    #[test]
    fn first_access_unit_emits_pat_and_pmt() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, false, 64));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1);
        assert_eq!(of_pid(&packets, PMT_PID).len(), 1);
    }

    #[test]
    fn keyframe_repeats_psi() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 64);
        let packets = parse_packets(&mux(&mut muxer, 33, true, 64));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1);
        assert_eq!(of_pid(&packets, PMT_PID).len(), 1);
    }

    #[test]
    fn p_frame_inside_the_interval_does_not_repeat_psi() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 64);
        let packets = parse_packets(&mux(&mut muxer, 499, false, 64));
        assert!(of_pid(&packets, PAT_PID).is_empty());
        assert!(of_pid(&packets, PMT_PID).is_empty());
    }

    #[test]
    fn psi_repeats_once_the_interval_elapses() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 64);
        let packets = parse_packets(&mux(&mut muxer, 500, false, 64));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1);
        assert_eq!(of_pid(&packets, PMT_PID).len(), 1);
    }

    #[test]
    fn keyframe_on_the_interval_boundary_does_not_duplicate_psi() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 64);
        let packets = parse_packets(&mux(&mut muxer, 600, true, 64));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1);
        assert_eq!(of_pid(&packets, PMT_PID).len(), 1);
    }

    #[test]
    fn regressive_pts_does_not_trigger_psi() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 1000, true, 64);
        let packets = parse_packets(&mux(&mut muxer, 100, false, 64));
        assert!(of_pid(&packets, PAT_PID).is_empty());
    }

    // --- PES ------------------------------------------------------------

    #[test]
    fn pes_uses_the_video_stream_id_with_pts_only() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 40, false, 64));
        let video = of_pid(&packets, VIDEO_PID);
        let payload = &video[0].payload;
        assert_eq!(&payload[..3], &[0x00, 0x00, 0x01]);
        assert_eq!(payload[3], PES_STREAM_ID_VIDEO);
        assert_eq!(
            u16::from_be_bytes([payload[4], payload[5]]),
            0,
            "PES_packet_length must be 0"
        );
        assert_eq!(payload[7] & 0xC0, 0x80, "PTS present, DTS absent");
        assert_eq!(payload[8], 5, "PES_header_data_length for PTS only");
        assert_eq!(payload[9] & 0xF0, 0x20, "PTS prefix must be '0010'");
    }

    #[test]
    fn pts_zero_is_encoded() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 64));
        assert_eq!(decode_pts(&packets), 0);
    }

    #[test]
    fn pts_of_one_second_is_ninety_thousand_ticks() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 1000, true, 64));
        assert_eq!(decode_pts(&packets), 90_000);
    }

    #[test]
    fn pts_near_the_wrap_is_encoded_without_wrapping() {
        // Largest millisecond value whose ×90 still fits in 33 bits.
        let pts_ms = ((1i64 << 33) - 90) / 90;
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, pts_ms, true, 64));
        let expected = (pts_ms * 90) as u64;
        assert!(expected < (1u64 << 33));
        assert_eq!(decode_pts(&packets), expected);
    }

    #[test]
    fn pts_wraps_at_thirty_three_bits() {
        let pts_ms = (1i64 << 33) / 90 + 1;
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, pts_ms, true, 64));
        let expected = ((pts_ms as i128 * 90) % (1i128 << 33)) as u64;
        assert!((pts_ms as i128 * 90) >= (1i128 << 33), "test must wrap");
        assert_eq!(decode_pts(&packets), expected);
    }

    #[test]
    fn only_the_first_video_packet_sets_pusi() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 3000));
        let video = of_pid(&packets, VIDEO_PID);
        assert!(video.len() > 2, "test needs a multi-packet PES");
        assert!(video[0].payload_unit_start);
        assert!(video[1..].iter().all(|p| !p.payload_unit_start));
    }

    // --- PCR and random access -----------------------------------------

    #[test]
    fn first_video_packet_carries_a_pcr_matching_the_pts() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 1234, false, 2000));
        let video = of_pid(&packets, VIDEO_PID);
        assert_eq!(video[0].pcr(), Some(1234 * 90));
        assert_eq!(video[0].pcr(), Some(decode_pts(&packets)));
        assert!(
            video[1..].iter().all(|p| p.pcr().is_none()),
            "PCR belongs on the first packet only"
        );
    }

    #[test]
    fn pcr_extension_is_zero_and_reserved_bits_are_set() {
        let pcr = encode_pcr(0x1_FFFF_FFFF);
        assert_eq!(pcr[5], 0x00, "PCR extension low bits");
        assert_eq!(pcr[4] & 0x7E, 0x7E, "six reserved bits must be 1");
        assert_eq!(pcr[4] & 0x01, 0x00, "PCR extension high bit");
    }

    #[test]
    fn keyframe_sets_random_access_and_p_frame_does_not() {
        let mut muxer = MpegTsMuxer::new();
        let key = parse_packets(&mux(&mut muxer, 0, true, 500));
        assert!(of_pid(&key, VIDEO_PID)[0].random_access());
        let inter = parse_packets(&mux(&mut muxer, 33, false, 500));
        assert!(!of_pid(&inter, VIDEO_PID)[0].random_access());
    }

    // --- discontinuity after reset() ------------------------------------

    #[test]
    fn a_new_muxer_announces_no_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        assert!(!muxer.snapshot().discontinuity_pending);
        assert!(!first_video(&mux(&mut muxer, 0, true, 500)).discontinuity());
    }

    #[test]
    fn ordinary_access_units_announce_no_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        for (index, pts) in [0i64, 33, 66, 500, 533].iter().enumerate() {
            let out = mux(&mut muxer, *pts, index == 0, 500);
            assert!(
                parse_packets(&out)
                    .iter()
                    .all(|packet| !packet.discontinuity()),
                "AU at {pts} ms announced a discontinuity"
            );
        }
    }

    #[test]
    fn reset_arms_the_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        assert!(!muxer.snapshot().discontinuity_pending);
        muxer.reset();
        assert!(muxer.snapshot().discontinuity_pending);
        assert_eq!(muxer.continuity_counters(), (0, 0, 0));
        assert_eq!(muxer.snapshot().last_psi_pts_ms, None);
    }

    #[test]
    fn first_access_unit_after_reset_sets_the_discontinuity_bit() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        muxer.reset();
        let out = mux(&mut muxer, 1000, true, 500);
        let packet = first_video(&out);
        assert!(
            packet.discontinuity(),
            "discontinuity_indicator must be set"
        );
        assert_eq!(
            packet.adaptation.as_ref().and_then(|f| f.first()).copied(),
            Some(AF_DISCONTINUITY | AF_RANDOM_ACCESS | AF_PCR),
            "discontinuity must not displace the PCR or random access flags"
        );
        assert_eq!(
            packet.pcr(),
            Some(1000 * 90),
            "PCR still present and correct"
        );
        assert!(packet.random_access(), "keyframe still flags random access");
        assert_eq!(packet.continuity, 0, "counters restart at zero");
    }

    #[test]
    fn p_frame_after_reset_announces_discontinuity_without_random_access() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        muxer.reset();
        let packet = first_video(&mux(&mut muxer, 1000, false, 500));
        assert!(packet.discontinuity());
        assert!(!packet.random_access());
        assert_eq!(
            packet.adaptation.as_ref().and_then(|f| f.first()).copied(),
            Some(AF_DISCONTINUITY | AF_PCR)
        );
    }

    #[test]
    fn only_the_first_packet_of_the_first_access_unit_announces_it() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        muxer.reset();
        // A multi-packet access unit: only its very first packet may carry it.
        let out = mux(&mut muxer, 1000, true, 3000);
        let packets = parse_packets(&out);
        let flagged: Vec<usize> = packets
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.discontinuity())
            .map(|(index, _)| index)
            .collect();
        let first_video_index = packets
            .iter()
            .position(|packet| packet.pid == VIDEO_PID)
            .expect("video packets exist");
        assert_eq!(flagged, vec![first_video_index]);
        assert!(!muxer.snapshot().discontinuity_pending, "cleared on commit");

        // And the next access unit is clean again.
        let next = parse_packets(&mux(&mut muxer, 1033, false, 3000));
        assert!(next.iter().all(|packet| !packet.discontinuity()));
    }

    #[test]
    fn a_second_reset_arms_it_again() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        muxer.reset();
        assert!(first_video(&mux(&mut muxer, 1000, true, 500)).discontinuity());
        assert!(!first_video(&mux(&mut muxer, 1033, false, 500)).discontinuity());
        muxer.reset();
        assert!(first_video(&mux(&mut muxer, 2000, true, 500)).discontinuity());
    }

    #[test]
    fn a_rejected_access_unit_does_not_consume_the_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 500);
        muxer.reset();
        let armed = muxer.snapshot();
        assert_eq!(
            muxer.mux_access_unit(0, true, &[]),
            Err(MpegTsError::EmptyAccessUnit)
        );
        assert_eq!(
            muxer.mux_access_unit(-1, true, &access_unit(500)),
            Err(MpegTsError::NegativePts)
        );
        assert_eq!(
            muxer.mux_access_unit(0, true, &[0x65, 0x88]),
            Err(MpegTsError::InvalidAnnexB)
        );
        assert_eq!(muxer.snapshot(), armed, "state must be untouched");
        assert!(first_video(&mux(&mut muxer, 1000, true, 500)).discontinuity());
    }

    // --- transactional state --------------------------------------------

    #[test]
    fn a_packetization_failure_rolls_the_whole_state_back() {
        let mut muxer = MpegTsMuxer::new();
        // Get well past the first access unit so every counter is mid-stream
        // and the snapshot is non-trivial.
        mux(&mut muxer, 0, true, 3000);
        mux(&mut muxer, 500, true, 3000);
        mux(&mut muxer, 533, false, 3000);
        let before = muxer.snapshot();
        assert_ne!(before.video_continuity, 0, "counters must be mid-stream");
        assert_ne!(before.pat_continuity, 0);

        muxer.fail_next_packetization = true;
        // A keyframe, so PSI would also have been emitted and the schedule
        // updated had the call been allowed to commit.
        assert_eq!(
            muxer.mux_access_unit(2000, true, &access_unit(3000)),
            Err(MpegTsError::InternalPacketization)
        );
        assert_eq!(muxer.snapshot(), before, "no field may have moved");

        muxer.fail_next_packetization = false;
        let out = mux(&mut muxer, 2000, true, 3000);
        let packets = parse_packets(&out);
        // The retry behaves exactly as the failed call would have: PSI is
        // still due and continuity picks up from where it really was.
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1);
        assert_eq!(
            of_pid(&packets, PAT_PID)[0].continuity,
            before.pat_continuity
        );
        assert_eq!(
            of_pid(&packets, PMT_PID)[0].continuity,
            before.pmt_continuity
        );
        let video = of_pid(&packets, VIDEO_PID);
        for (offset, packet) in video.iter().enumerate() {
            let expected = (before.video_continuity as usize + offset) & 0x0F;
            assert_eq!(packet.continuity as usize, expected);
        }
    }

    #[test]
    fn a_packetization_failure_does_not_consume_the_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 3000);
        muxer.reset();
        let armed = muxer.snapshot();
        assert!(armed.discontinuity_pending);

        muxer.fail_next_packetization = true;
        assert_eq!(
            muxer.mux_access_unit(1000, true, &access_unit(3000)),
            Err(MpegTsError::InternalPacketization)
        );
        assert_eq!(muxer.snapshot(), armed, "the discontinuity must survive");

        muxer.fail_next_packetization = false;
        let packet = first_video(&mux(&mut muxer, 1000, true, 3000));
        assert!(packet.discontinuity(), "the retry must still announce it");
        assert_eq!(packet.continuity, 0);
    }

    #[test]
    fn a_packetization_failure_leaves_the_psi_schedule_alone() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 3000);
        let before = muxer.snapshot();
        assert_eq!(before.last_psi_pts_ms, Some(0));

        muxer.fail_next_packetization = true;
        // 700 ms later a P-frame would have re-emitted PSI and moved the
        // schedule to 700; the failure must not let that stick.
        assert!(muxer
            .mux_access_unit(700, false, &access_unit(3000))
            .is_err());
        assert_eq!(muxer.snapshot().last_psi_pts_ms, Some(0));
        assert_eq!(muxer.snapshot(), before);

        muxer.fail_next_packetization = false;
        let packets = parse_packets(&mux(&mut muxer, 700, false, 3000));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1, "PSI was still due");
    }

    // --- payload fidelity ------------------------------------------------

    #[test]
    fn reconstructed_payload_matches_the_access_unit() {
        let mut muxer = MpegTsMuxer::new();
        let unit = access_unit(300);
        let packets = parse_packets(&muxer.mux_access_unit(0, true, &unit).expect("should mux"));
        assert_eq!(reconstruct_annexb(&packets), unit);
    }

    #[test]
    fn large_access_unit_is_reconstructed_exactly() {
        let mut muxer = MpegTsMuxer::new();
        let mut unit = access_unit(120_000);
        // Vary the bytes so a mis-ordered reassembly cannot pass.
        for (index, byte) in unit.iter_mut().enumerate().skip(5) {
            *byte = (index % 251) as u8;
        }
        let packets = parse_packets(
            &muxer
                .mux_access_unit(5_000, true, &unit)
                .expect("should mux"),
        );
        let video = of_pid(&packets, VIDEO_PID);
        assert!(video.len() > 600, "expected hundreds of packets");
        assert_eq!(reconstruct_annexb(&packets), unit);
    }

    #[test]
    fn payload_sizes_around_every_packet_boundary_round_trip() {
        // The first packet holds 176 body bytes (14 of them the PES header),
        // every later one 184 — so walk both boundaries and their neighbours.
        let mut sizes: Vec<usize> = (5..400).collect();
        for packets in 1..8usize {
            let exact = FIRST_PACKET_CAPACITY + packets * TS_BODY_SIZE - PES_HEADER_LEN;
            sizes.extend([exact - 1, exact, exact + 1]);
        }
        for size in sizes {
            let mut muxer = MpegTsMuxer::new();
            let unit = access_unit(size);
            let out = muxer
                .mux_access_unit(0, true, &unit)
                .unwrap_or_else(|e| panic!("size {size} failed: {e}"));
            assert_eq!(out.len() % TS_PACKET_SIZE, 0, "size {size}");
            assert_eq!(
                reconstruct_annexb(&parse_packets(&out)),
                unit,
                "size {size}"
            );
        }
    }

    #[test]
    fn input_is_not_modified() {
        let unit = access_unit(500);
        let snapshot = unit.clone();
        let mut muxer = MpegTsMuxer::new();
        muxer.mux_access_unit(0, true, &unit).expect("should mux");
        assert_eq!(unit, snapshot);
    }

    // --- capacity --------------------------------------------------------

    #[test]
    fn reserved_capacity_matches_what_is_actually_emitted() {
        // The reservation is only an allocation hint, so the real check is
        // that it never changes the bytes and never under-reserves.
        for (size, keyframe) in [(5usize, true), (176, false), (177, true), (5000, false)] {
            let mut reserved = MpegTsMuxer::new();
            let unit = access_unit(size.max(5));
            let out = reserved.mux_access_unit(0, keyframe, &unit).expect("mux");
            let pes_len = PES_HEADER_LEN + unit.len();
            let capacity = output_capacity(pes_len, true).expect("capacity");
            assert_eq!(capacity, out.len(), "size {size}: capacity should be exact");
            assert_eq!(reconstruct_annexb(&parse_packets(&out)), unit);
        }
    }

    #[test]
    fn capacity_arithmetic_cannot_overflow() {
        // usize::MAX bytes of PES would need ~1.0e17 packets, i.e. ~1.9e19
        // bytes of output — past usize::MAX, so the multiply must be caught.
        assert!(output_capacity(usize::MAX, true).is_err());
        assert!(output_capacity(usize::MAX - 1, false).is_err());
        // Merely enormous still computes, without panicking or wrapping.
        assert!(output_capacity(usize::MAX / 200, true).is_ok());
        assert_eq!(output_capacity(1, false), Ok(TS_PACKET_SIZE));
        assert_eq!(output_capacity(1, true), Ok(3 * TS_PACKET_SIZE));
        assert_eq!(
            output_capacity(FIRST_PACKET_CAPACITY + 1, false),
            Ok(2 * TS_PACKET_SIZE)
        );
    }

    // --- continuity counters ---------------------------------------------

    #[test]
    fn video_continuity_increments_per_packet() {
        let mut muxer = MpegTsMuxer::new();
        let packets = parse_packets(&mux(&mut muxer, 0, true, 2000));
        let video = of_pid(&packets, VIDEO_PID);
        for (index, packet) in video.iter().enumerate() {
            assert_eq!(packet.continuity, (index as u8) & 0x0F);
        }
    }

    #[test]
    fn video_continuity_wraps_from_fifteen_to_zero() {
        let mut muxer = MpegTsMuxer::new();
        // 20 video packets: enough to cross the 4-bit wrap once.
        let packets = parse_packets(&mux(&mut muxer, 0, true, 20 * TS_BODY_SIZE));
        let counters: Vec<u8> = of_pid(&packets, VIDEO_PID)
            .iter()
            .map(|packet| packet.continuity)
            .collect();
        assert!(counters.len() > 16, "test needs more than 16 packets");
        assert_eq!(counters[15], 15);
        assert_eq!(counters[16], 0);
    }

    #[test]
    fn psi_counters_are_independent_of_each_other_and_of_video() {
        let mut muxer = MpegTsMuxer::new();
        // 500 bytes → a 3-packet PES, then a second access unit of 1 packet:
        // 4 video packets against 2 PSI emissions of one packet each.
        let first = parse_packets(&mux(&mut muxer, 0, true, 500));
        assert_eq!(of_pid(&first, VIDEO_PID).len(), 3);
        let packets = parse_packets(&mux(&mut muxer, 1000, true, 64));
        assert_eq!(of_pid(&packets, PAT_PID)[0].continuity, 1);
        assert_eq!(of_pid(&packets, PMT_PID)[0].continuity, 1);
        assert_eq!(of_pid(&packets, VIDEO_PID)[0].continuity, 3);
        assert_eq!(
            muxer.continuity_counters(),
            (2, 2, 4),
            "each PID counts only its own packets"
        );
    }

    #[test]
    fn every_emitted_packet_carries_payload_so_continuity_always_advances() {
        let mut muxer = MpegTsMuxer::new();
        let out = mux(&mut muxer, 0, true, 900);
        for packet in parse_packets(&out) {
            assert!(
                !packet.payload.is_empty(),
                "a payload-less packet would break the continuity contract"
            );
        }
    }

    #[test]
    fn reset_restores_the_counters() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 2000);
        assert_ne!(muxer.continuity_counters(), (0, 0, 0));
        muxer.reset();
        assert_eq!(muxer.continuity_counters(), (0, 0, 0));
    }

    #[test]
    fn reset_forces_psi_on_the_next_access_unit() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 64);
        let without_reset = parse_packets(&mux(&mut muxer, 10, false, 64));
        assert!(of_pid(&without_reset, PAT_PID).is_empty());
        muxer.reset();
        let after_reset = parse_packets(&mux(&mut muxer, 20, false, 64));
        assert_eq!(of_pid(&after_reset, PAT_PID).len(), 1);
        assert_eq!(of_pid(&after_reset, PMT_PID).len(), 1);
        assert_eq!(of_pid(&after_reset, PAT_PID)[0].continuity, 0);
        assert_eq!(of_pid(&after_reset, VIDEO_PID)[0].continuity, 0);
    }

    #[test]
    fn continuity_continues_normally_after_the_discontinuity() {
        let mut muxer = MpegTsMuxer::new();
        mux(&mut muxer, 0, true, 2000);
        muxer.reset();
        let mut counters = Vec::new();
        for (index, pts) in [1000i64, 1033, 1066].iter().enumerate() {
            let out = mux(&mut muxer, *pts, index == 0, 2000);
            for packet in parse_packets(&out) {
                if packet.pid == VIDEO_PID {
                    counters.push(packet.continuity);
                }
            }
        }
        for (index, counter) in counters.iter().enumerate() {
            assert_eq!(*counter, (index as u8) & 0x0F, "packet {index}");
        }
    }

    // --- input validation -------------------------------------------------

    #[test]
    fn empty_access_unit_is_rejected() {
        let mut muxer = MpegTsMuxer::new();
        assert_eq!(
            muxer.mux_access_unit(0, true, &[]),
            Err(MpegTsError::EmptyAccessUnit)
        );
    }

    #[test]
    fn both_start_code_lengths_are_accepted() {
        let mut muxer = MpegTsMuxer::new();
        assert!(muxer.mux_access_unit(0, true, &[0, 0, 1, 0x65]).is_ok());
        assert!(muxer.mux_access_unit(0, true, &[0, 0, 0, 1, 0x65]).is_ok());
    }

    #[test]
    fn leading_zero_bytes_before_the_start_code_are_accepted() {
        let mut muxer = MpegTsMuxer::new();
        // One extra zero, then several: H.264 Annex B allows any number of
        // leading_zero_8bits in front of the start code.
        assert!(muxer
            .mux_access_unit(0, true, &[0, 0, 0, 0, 1, 0x65, 0x88])
            .is_ok());
        assert!(muxer
            .mux_access_unit(0, true, &[0, 0, 0, 0, 0, 0, 1, 0x65, 0x88])
            .is_ok());
    }

    #[test]
    fn leading_zero_bytes_are_carried_through_untouched() {
        let mut muxer = MpegTsMuxer::new();
        let mut unit = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x65];
        unit.extend((0..600).map(|index| (index % 251) as u8));
        let packets = parse_packets(&muxer.mux_access_unit(0, true, &unit).expect("mux"));
        let recovered = reconstruct_annexb(&packets);
        assert_eq!(recovered, unit, "the muxer must not strip anything");
        assert_eq!(&recovered[..6], &[0x00, 0x00, 0x00, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn non_annexb_input_is_rejected() {
        let mut muxer = MpegTsMuxer::new();
        for input in [
            vec![0x65, 0x88, 0x84],             // no start code
            vec![0xFF, 0x00, 0x00, 0x01, 0x65], // non-zero byte in front
            vec![0x00, 0x00, 0x01],             // start code with no NAL
            vec![0x00, 0x00, 0x00, 0x01],       // 4-byte start code, no NAL
            vec![0x00, 0x00, 0x00, 0x00, 0x01], // padded start code, no NAL
            vec![0x00, 0x00, 0x02, 0x01, 0x65], // not a start code
            vec![0x00, 0x01, 0x65],             // only one leading zero
            vec![0x00],
            vec![0x00, 0x00],
            vec![0x00, 0x00, 0x00],
            vec![0x00; 64], // nothing but zeros
        ] {
            assert_eq!(
                muxer.mux_access_unit(0, true, &input),
                Err(MpegTsError::InvalidAnnexB),
                "should reject {input:02X?}"
            );
        }
    }

    #[test]
    fn negative_pts_is_rejected() {
        let mut muxer = MpegTsMuxer::new();
        assert_eq!(
            muxer.mux_access_unit(-1, true, &access_unit(64)),
            Err(MpegTsError::NegativePts)
        );
        assert_eq!(
            muxer.mux_access_unit(i64::MIN, true, &access_unit(64)),
            Err(MpegTsError::NegativePts)
        );
    }

    #[test]
    fn rejections_leave_no_state_behind() {
        let mut muxer = MpegTsMuxer::new();
        let initial = muxer.snapshot();
        let _ = muxer.mux_access_unit(-1, true, &access_unit(64));
        let _ = muxer.mux_access_unit(0, true, &[]);
        let _ = muxer.mux_access_unit(0, true, &[0x00; 8]);
        assert_eq!(muxer.snapshot(), initial);
        let packets = parse_packets(&mux(&mut muxer, 0, false, 64));
        assert_eq!(of_pid(&packets, PAT_PID).len(), 1, "PSI still due");
    }

    #[test]
    fn extreme_pts_does_not_panic() {
        let mut muxer = MpegTsMuxer::new();
        assert!(muxer
            .mux_access_unit(i64::MAX, true, &access_unit(64))
            .is_ok());
    }

    // --- CRC ---------------------------------------------------------------

    #[test]
    fn crc32_mpeg2_matches_the_reference_vector() {
        assert_eq!(crc32_mpeg2(b"123456789"), 0x0376_E6E7);
        assert_eq!(crc32_mpeg2(&[]), 0xFFFF_FFFF);
    }

    // --- deterministic end-to-end sequence ----------------------------------

    #[test]
    fn deterministic_sequence_report() {
        let mut muxer = MpegTsMuxer::new();
        let sequence = [(0i64, true), (33, false), (66, false), (500, false)];
        let mut out = Vec::new();
        for (pts_ms, keyframe) in sequence {
            out.extend_from_slice(&mux(&mut muxer, pts_ms, keyframe, 100));
        }
        let packets = parse_packets(&out);
        let pat = of_pid(&packets, PAT_PID);
        let pmt = of_pid(&packets, PMT_PID);
        let video = of_pid(&packets, VIDEO_PID);

        // PSI at the first access unit (keyframe) and again at 500 ms.
        assert_eq!(pat.len(), 2);
        assert_eq!(pmt.len(), 2);
        assert_eq!(video.len(), 4);
        assert_eq!(pat[0].continuity, 0);
        assert_eq!(pmt[0].continuity, 0);
        assert_eq!(video[0].continuity, 0);

        let decoded_pts: Vec<u64> = video
            .iter()
            .map(|packet| {
                let bytes = &packet.payload[9..14];
                (((bytes[0] as u64) & 0x0E) << 29)
                    | ((bytes[1] as u64) << 22)
                    | (((bytes[2] as u64) & 0xFE) << 14)
                    | ((bytes[3] as u64) << 7)
                    | ((bytes[4] as u64) >> 1)
            })
            .collect();
        let decoded_pcr: Vec<Option<u64>> = video.iter().map(|packet| packet.pcr()).collect();
        assert_eq!(decoded_pts, vec![0, 2_970, 5_940, 45_000]);
        assert_eq!(
            decoded_pcr,
            vec![Some(0), Some(2_970), Some(5_940), Some(45_000)]
        );
        assert!(video[0].random_access());
        assert!(video[1..].iter().all(|packet| !packet.random_access()));
        assert!(packets.iter().all(|packet| !packet.discontinuity()));

        // Each access unit is its own PES here, so reconstruct one of them.
        let first = parse_packets(&out[..3 * TS_PACKET_SIZE]);
        assert_eq!(reconstruct_annexb(&first).len(), 100);

        println!("--- deterministic sequence (PTS 0 key, 33, 66, 500) ---");
        println!("PAT packets:   {}", pat.len());
        println!("PMT packets:   {}", pmt.len());
        println!("VIDEO packets: {}", video.len());
        println!(
            "first continuity: PAT={} PMT={} VIDEO={}",
            pat[0].continuity, pmt[0].continuity, video[0].continuity
        );
        println!("decoded PTS (90 kHz): {decoded_pts:?}");
        println!("decoded PCR (90 kHz): {decoded_pcr:?}");
        println!(
            "reconstructed Annex-B of AU #1: {} bytes",
            reconstruct_annexb(&first).len()
        );
        println!("total packets: {}", packets.len());
    }
}
