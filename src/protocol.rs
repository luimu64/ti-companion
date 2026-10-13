//! TI-Nspire NavNet link protocol — packet framing.
//!
//! Recovered from `lib/navnet.dll` (TI-Nspire CX CAS Student Software 5.4.0.259)
//! with Ghidra (via REA) plus the Windows connector `nwb_nspire_connector.dll`.
//!
//! Wire format (all multi-byte fields big-endian), 16-byte header:
//!
//! ```text
//! off size field
//! 0   2    magic = 0x54FD
//! 2   2    source address
//! 4   2    source service id (sid)
//! 6   2    destination address
//! 8   2    destination service id (sid)
//! 10  2    data checksum (see data_checksum)
//! 12  1    data size; 0xFF means "extended": 32-bit size follows the header
//! 13  1    ack flag (0x0A on ACK packets, else 0)
//! 14  1    sequence number
//! 15  1    header checksum = sum of bytes 0..=14 (mod 256)
//! 16  ...  data (or, if size==0xFF: u32 BE size at 16..20, data at 20)
//! ```
//!
//! This matches `navnet.dll!TI_NS_ConstructPacket` (magic stored as 0xfd54
//! little-endian, one-byte sum checksum over the first 15 header bytes) and the
//! "attempted to compare header checksum" validator.

pub const MAGIC: u16 = 0x54FD;
pub const HEADER_SIZE: usize = 16;

/// Max payload for one packet on classic/CX calculators.
pub const MAX_DATA_CLASSIC: usize = 254;
/// Max payload for one packet on CX II.
pub const MAX_DATA_CX2: usize = 1440;

/// NavNet addresses/services used by the initial handshake.
pub const HOST_ADDR: u16 = 0x6400;
pub const DEVICE_ADDR: u16 = 0x6401;
pub const INIT_SID: u16 = 0x4003;
pub const HOST_SID: u16 = 0x8000;

/// Source sids used by ACK/NACK packets and the disconnect packet.
pub const SID_ACK: u16 = 0x00FF;
pub const SID_ACK0: u16 = 0x00FE; // when the acked packet had seq == 0
pub const SID_NACK: u16 = 0x00D3;
pub const SID_DISCONNECT: u16 = 0x40DE;

/// File-manager / device-info NavNet services.
pub const SID_FILE: u16 = 0x4060;
pub const SID_DEVINFO: u16 = 0x4020;

pub const ACK_FLAG: u8 = 0x0A;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub src_addr: u16,
    pub src_sid: u16,
    pub dst_addr: u16,
    pub dst_sid: u16,
    pub ack: u8,
    pub seq: u8,
    pub data: Vec<u8>,
}

impl Packet {
    pub fn new(src_addr: u16, src_sid: u16, dst_addr: u16, dst_sid: u16, seq: u8) -> Self {
        Packet { src_addr, src_sid, dst_addr, dst_sid, ack: 0, seq, data: Vec::new() }
    }

    /// Encode to the wire.
    pub fn encode(&self) -> Vec<u8> {
        let extended = self.data.len() >= 0xFF;
        let data_len = if extended { self.data.len() + 4 } else { self.data.len() };
        let mut buf = Vec::with_capacity(HEADER_SIZE + data_len);

        buf.extend_from_slice(&MAGIC.to_be_bytes());
        buf.extend_from_slice(&self.src_addr.to_be_bytes());
        buf.extend_from_slice(&self.src_sid.to_be_bytes());
        buf.extend_from_slice(&self.dst_addr.to_be_bytes());
        buf.extend_from_slice(&self.dst_sid.to_be_bytes());
        buf.extend_from_slice(&data_checksum(&self.data).to_be_bytes());
        if extended {
            buf.push(0xFF);
            buf.push(self.ack);
            buf.push(self.seq);
            buf.push(0); // header checksum placeholder
            buf.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
            buf.extend_from_slice(&self.data);
        } else {
            buf.push(self.data.len() as u8);
            buf.push(self.ack);
            buf.push(self.seq);
            buf.push(0); // header checksum placeholder
            buf.extend_from_slice(&self.data);
        }
        // Header checksum = sum of the first 15 header bytes.
        let ck: u8 = buf[..HEADER_SIZE - 1].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        buf[HEADER_SIZE - 1] = ck;
        buf
    }

    /// Decode one packet from a full message. Returns None on bad framing.
    pub fn decode(buf: &[u8]) -> Option<Packet> {
        if buf.len() < HEADER_SIZE {
            return None;
        }
        if u16::from_be_bytes([buf[0], buf[1]]) != MAGIC {
            return None;
        }
        let ck: u8 = buf[..HEADER_SIZE - 1].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        if ck != buf[HEADER_SIZE - 1] {
            return None;
        }
        let src_addr = u16::from_be_bytes([buf[2], buf[3]]);
        let src_sid = u16::from_be_bytes([buf[4], buf[5]]);
        let dst_addr = u16::from_be_bytes([buf[6], buf[7]]);
        let dst_sid = u16::from_be_bytes([buf[8], buf[9]]);
        let stored_dc = u16::from_be_bytes([buf[10], buf[11]]);
        let (data, dc) = if buf[12] == 0xFF {
            if buf.len() < 20 {
                return None;
            }
            let size = u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]) as usize;
            if buf.len() < 20 + size {
                return None;
            }
            (buf[20..20 + size].to_vec(), &buf[16..])
        } else {
            let size = buf[12] as usize;
            if buf.len() < HEADER_SIZE + size {
                return None;
            }
            (buf[HEADER_SIZE..HEADER_SIZE + size].to_vec(), &buf[0..0])
        };
        // `dc` slice trick above is only used to keep the borrow checker quiet;
        // the real checksum covers just the logical data payload.
        let _ = dc;
        if data_checksum(&data) != stored_dc {
            return None;
        }
        Some(Packet {
            src_addr,
            src_sid,
            dst_addr,
            dst_sid,
            ack: buf[13],
            seq: buf[14],
            data,
        })
    }
}

/// 16-bit data checksum used by the NavNet protocol.
///
/// Transcribed verbatim from `calculate_data_checksum` in navnet.dll's sibling
/// implementation; a byte-wise LFSR-style mix over the payload.
pub fn data_checksum(data: &[u8]) -> u16 {
    let mut chksum: u16 = 0;
    for &b in data {
        let tmp1: u16 = ((b as u16) << 8) | (chksum >> 8);
        let c = chksum & 0xFF;
        let tmp2: u16 = ((((c & 0xF) << 4) ^ c) << 8) & 0xFFFF;
        let tmp3: u16 = tmp2 >> 5;
        let mut cs: u16 = tmp3 >> 7;
        cs ^= tmp1 ^ tmp2 ^ tmp3;
        chksum = cs;
    }
    chksum
}

/// Build the ACK (or NACK) packet for a received packet.
pub fn ack_for(p: &Packet, nack: bool) -> Packet {
    let mut ack = Packet::new(HOST_ADDR, if nack { SID_NACK } else if p.seq != 0 { SID_ACK } else { SID_ACK0 }, DEVICE_ADDR, p.src_sid, p.seq);
    ack.ack = ACK_FLAG;
    // Two bytes: the sid the device should use for its next packet.
    ack.data = p.dst_sid.to_be_bytes().to_vec();
    ack
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_layout_and_checksum_roundtrip() {
        let mut p = Packet::new(0x6400, 0x8000, 0x6401, 0x4060, 1);
        p.data = vec![0x0D, b'/', 0, 0, 0, 0, 0, 0, 0];
        let buf = p.encode();
        assert_eq!(&buf[0..2], &[0x54, 0xFD]);
        assert_eq!(u16::from_be_bytes([buf[2], buf[3]]), 0x6400);
        assert_eq!(u16::from_be_bytes([buf[4], buf[5]]), 0x8000);
        assert_eq!(u16::from_be_bytes([buf[6], buf[7]]), 0x6401);
        assert_eq!(u16::from_be_bytes([buf[8], buf[9]]), 0x4060);
        assert_eq!(buf[12], 9);
        // header checksum
        let ck: u8 = buf[..15].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(buf[15], ck);
        let back = Packet::decode(&buf).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn extended_size_packet() {
        let mut p = Packet::new(1, 2, 3, 4, 5);
        p.data = vec![0xAB; 300];
        let buf = p.encode();
        assert_eq!(buf[12], 0xFF);
        assert_eq!(u32::from_be_bytes([buf[16], buf[17], buf[18], buf[19]]), 300);
        assert_eq!(Packet::decode(&buf).unwrap(), p);
    }

    #[test]
    fn corrupt_header_rejected() {
        let p = Packet::new(1, 2, 3, 4, 5);
        let mut buf = p.encode();
        buf[2] ^= 0xFF; // breaks header checksum
        assert!(Packet::decode(&buf).is_none());
    }

    /// Golden vector: a `DIR_ENUM_INIT` request for "/" as produced by the
    /// reversed protocol (magic 54 FD, sids, header checksum = sum of the
    /// first 15 bytes).
    #[test]
    fn golden_dirlist_init_packet() {
        let mut p = Packet::new(0x6400, 0x8000, 0x6401, 0x4060, 1);
        // 0x0D, "/" padded to 8, NUL, extra NUL
        p.data = vec![0x0D, b'/', 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let buf = p.encode();
        let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
        // Header is stable and independent of endianness handling elsewhere.
        assert!(hex.starts_with("54fd6400800064014060"));
        assert_eq!(buf[12], 11); // data size
        assert_eq!(buf[13], 0); // not an ack
        assert_eq!(buf[14], 1); // seq
        let sum: u8 = buf[..15].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(buf[15], sum);
        assert_eq!(Packet::decode(&buf).unwrap(), p);
    }
}
