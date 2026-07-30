use std::{cmp::Ordering, fmt};

/// Serialied, it looks like:
/// major * 0x10000 + minor * 0x100 + patch
#[derive(PartialEq, Eq, Clone, Copy)]
pub struct SrtVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u8,
}

impl SrtVersion {
    /// **Sehcontrol patch — upstream srt-protocol 0.4.4 declares 1.3.1 here.**
    ///
    /// This is the only change this vendored copy makes to the crate; the
    /// remaining 74 files are byte-identical to the published 0.4.4.
    ///
    /// MediaMTX 1.9.3 embeds `datarhei/gosrt` v0.7.0, whose `DefaultConfig` sets
    /// `MinVersion = SRT_VERSION = 0x010401`, and whose `Validate` refuses any
    /// other minimum. A caller announcing 1.3.1 in its HSv5 `SRT_CMD_HSREQ`
    /// extension is therefore rejected during the handshake with
    /// `REJ_VERSION` (0x03F0 / 1008), before the stream id — and so before the
    /// publish token — is ever looked at.
    ///
    /// Raising the announced version does not overstate what this crate can do.
    /// Everything SRT added between 1.3.1 and 1.4.1 is negotiated through the
    /// capability bits in `SrtShakeFlags`, not implied by this field: the
    /// packet filter / FEC of 1.4 has its own `PACKET_FILTER` bit, which
    /// `SrtShakeFlags::SUPPORTED` does not set and this patch does not touch.
    /// The version field stays what peers actually use it for — a minimum-level
    /// gate — while the flags remain the honest statement of capability.
    ///
    /// Interop with MediaMTX past the handshake is *not* proven by this
    /// constant; see docs and the publisher's own diagnostics.
    pub const CURRENT: SrtVersion = SrtVersion {
        major: 1,
        minor: 4,
        patch: 1,
    };

    /// Create a new SRT version
    pub fn new(major: u8, minor: u8, patch: u8) -> SrtVersion {
        SrtVersion {
            major,
            minor,
            patch,
        }
    }

    /// Parse from an u32
    pub fn parse(from: u32) -> SrtVersion {
        let [_, major, minor, patch] = from.to_be_bytes();
        SrtVersion {
            major,
            minor,
            patch,
        }
    }

    /// Convert to an u32
    pub fn to_u32(self) -> u32 {
        u32::from(self.major) * 0x10000 + u32::from(self.minor) * 0x100 + u32::from(self.patch)
    }
}

impl PartialOrd for SrtVersion {
    fn partial_cmp(&self, other: &SrtVersion) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SrtVersion {
    fn cmp(&self, other: &SrtVersion) -> Ordering {
        match self.major.cmp(&other.major) {
            Ordering::Equal => match self.minor.cmp(&other.minor) {
                Ordering::Equal => self.patch.cmp(&other.patch),
                o => o,
            },
            o => o,
        }
    }
}

impl fmt::Display for SrtVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl fmt::Debug for SrtVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{self}")
    }
}

#[cfg(test)]
mod test {
    use super::SrtVersion;
    #[test]
    fn test_parse() {
        assert_eq!(SrtVersion::parse(0x01_01_01), SrtVersion::new(1, 1, 1));
        assert_eq!(SrtVersion::parse(0x00_00_00), SrtVersion::new(0, 0, 0));
    }

    #[test]
    fn test_display_debug() {
        assert_eq!(format!("{}", SrtVersion::new(12, 12, 12)), "12.12.12");
        assert_eq!(format!("{:?}", SrtVersion::new(12, 12, 12)), "12.12.12");
    }

    /// Guards the Sehcontrol patch. If a dependency bump silently restores
    /// upstream's 1.3.1, MediaMTX starts answering `REJ_VERSION` again and the
    /// preview fails with nothing but a generic connect error — this test is
    /// what turns that into a build failure instead.
    #[test]
    fn current_announces_the_version_mediamtx_accepts() {
        assert_eq!(SrtVersion::CURRENT, SrtVersion::new(1, 4, 1));
        assert_eq!(SrtVersion::CURRENT.to_u32(), 0x01_04_01);
        assert_eq!(SrtVersion::parse(0x01_04_01), SrtVersion::CURRENT);
        // gosrt compares numerically against its own 0x010401, so what matters
        // is that we are not below it.
        assert!(SrtVersion::CURRENT >= SrtVersion::new(1, 4, 1));
        assert!(SrtVersion::CURRENT > SrtVersion::new(1, 3, 1));
    }

    #[test]
    fn the_wire_encoding_round_trips_for_the_announced_version() {
        let encoded = SrtVersion::CURRENT.to_u32();
        assert_eq!(encoded.to_be_bytes(), [0x00, 0x01, 0x04, 0x01]);
        assert_eq!(SrtVersion::parse(encoded), SrtVersion::CURRENT);
    }
}
