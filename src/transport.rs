//! USB transport for the TI-Nspire.
//!
//! Two transports share the same NavNet packets:
//!  * classic Nspire (and CX): raw NavNet packets as USB bulk transfers
//!    (VID 0x0451, PID 0xE012), matching `nwb_nspire_connector.dll`'s
//!    `UsbIoWin`/`WinUsb_ReadPipe`/`WritePipe` usage.
//!  * CX II: NavNet packets wrapped in "NavNet SE" (NNSE) frames
//!    (VID 0x0451, PID 0xE022), matching `cx2.cpp` in libnspire.

use crate::protocol::{self, Packet, HEADER_SIZE, MAX_DATA_CLASSIC, MAX_DATA_CX2};
use rusb::{Context, DeviceHandle, UsbContext};
use std::time::Duration;

pub const VID: u16 = 0x0451;
pub const PID_CLASSIC: u16 = 0xE012;
pub const PID_CX2: u16 = 0xE022;
const TIMEOUT: Duration = Duration::from_millis(10_000);

/// NNSE service ids (CX II).
const NNSE_ADDR_ME: u8 = 0xFE;
const NNSE_ADDR_CALC: u8 = 0x01;
const NNSE_SVC_TIME: u8 = 0x02;
const NNSE_SVC_STREAM: u8 = 0x04;
const NNSE_SVC_UNKNOWN: u8 = 0x08;
const NNSE_ACK_FLAG: u8 = 0x80;

#[derive(Debug)]
pub enum TransportError {
    Usb(rusb::Error),
    NoDevice,
    Protocol(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Usb(e) => write!(f, "USB error: {e}"),
            TransportError::NoDevice => write!(f, "no TI-Nspire found on USB"),
            TransportError::Protocol(s) => write!(f, "protocol error: {s}"),
        }
    }
}

impl std::error::Error for TransportError {}
impl From<rusb::Error> for TransportError {
    fn from(e: rusb::Error) -> Self {
        TransportError::Usb(e)
    }
}

pub enum Link {
    Classic(UsbLink),
    Cx2(Cx2Link),
}

impl Link {
    /// Find and open a connected calculator.
    pub fn open() -> Result<Link, TransportError> {
        match UsbLink::open(PID_CLASSIC) {
            Ok(u) => Ok(Link::Classic(u)),
            Err(_) => {
                let u = UsbLink::open(PID_CX2)?;
                Ok(Link::Cx2(Cx2Link::new(u)))
            }
        }
    }

    pub fn is_cx2(&self) -> bool {
        matches!(self, Link::Cx2(_))
    }

    pub fn max_data(&self) -> usize {
        if self.is_cx2() { MAX_DATA_CX2 } else { MAX_DATA_CLASSIC }
    }
}

/// Shared USB plumbing: open the device, claim interface 0, discover endpoints.
pub struct UsbLink {
    handle: DeviceHandle<Context>,
    ep_in: u8,
    ep_out: u8,
    /// Classic transport sequence number (owned here, advanced on each ACK).
    pub seq: u8,
}

impl UsbLink {
    fn open(pid: u16) -> Result<UsbLink, TransportError> {
        let ctx = Context::new()?;
        let handle = ctx.open_device_with_vid_pid(VID, pid).ok_or(TransportError::NoDevice)?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        let _ = handle.set_active_configuration(1);
        let _ = handle.reset();
        // Interface 0 may be taken by another driver; ignore failure here and
        // let the bulk transfer report a real error.
        let _ = handle.claim_interface(0);

        // Endpoints can differ in recovery mode, so discover them.
        let device = handle.device();
        let config = device.active_config_descriptor()?;
        let mut ep_in = 0u8;
        let mut ep_out = 0u8;
        'ep: for iface in config.interfaces() {
            for desc in iface.descriptors() {
                for ep in desc.endpoint_descriptors() {
                    let addr = ep.address();
                    if addr & 0x80 != 0 {
                        if ep_in == 0 {
                            ep_in = addr;
                        }
                    } else if ep_out == 0 {
                        ep_out = addr;
                    }
                    if ep_in != 0 && ep_out != 0 {
                        break 'ep;
                    }
                }
            }
        }
        if ep_in == 0 || ep_out == 0 {
            return Err(TransportError::Protocol("device has no bulk endpoints".into()));
        }
        Ok(UsbLink {
            handle,
            ep_in,
            ep_out,
            seq: 1,
        })
    }

    fn write_raw(&self, buf: &[u8]) -> Result<(), TransportError> {
        if trace_enabled() {
            eprintln!("OUT {} : {}", buf.len(), hex(buf));
        }
        let n = self.handle.write_bulk(self.ep_out, buf, TIMEOUT)?;
        if n != buf.len() {
            return Err(TransportError::Protocol(format!("short write {}/{}", n, buf.len())));
        }
        Ok(())
    }

    fn read_raw(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let n = self.handle.read_bulk(self.ep_in, buf, TIMEOUT)?;
        if trace_enabled() {
            eprintln!("IN  {} : {}", n, hex(&buf[..n]));
        }
        Ok(n)
    }

    /// Classic handshake: wait for the device's address request, then assign
    /// the host address, then move to sid 0x8000.
    pub fn classic_init(&mut self) -> Result<(), TransportError> {
        // Wait for the address request packet (contents unused).
        let mut buf = vec![0u8; HEADER_SIZE + MAX_DATA_CLASSIC];
        self.read_raw(&mut buf).map_err(|_| TransportError::Protocol("no address request from device".into()))?;

        let mut p = Packet::new(protocol::HOST_ADDR, protocol::INIT_SID, protocol::DEVICE_ADDR, protocol::INIT_SID, 1);
        p.data = vec![0x64, 0x01, 0xFF, 0x00];
        self.write_raw(&p.encode())?;
        self.seq = 1;
        Ok(())
    }

    /// Send a packet and wait for its ACK (classic transport only).
    pub fn send_packet(&mut self, p: Packet, host_sid: u16) -> Result<(), TransportError> {
        let next = classic_send(self, self.seq, p, host_sid)?;
        self.seq = next;
        Ok(())
    }

    /// Receive a data packet addressed to the host, ACKing it.
    pub fn recv_packet(&mut self, maxlen: usize, host_sid: u16) -> Result<Packet, TransportError> {
        classic_recv(self, maxlen, host_sid)
    }
}

/// Raw byte pipe, so the classic packet exchange can be driven by a fake in
/// tests without USB hardware.
trait Pipe {
    fn write_raw(&self, buf: &[u8]) -> Result<(), TransportError>;
    fn read_raw(&self, buf: &mut [u8]) -> Result<usize, TransportError>;
}

impl Pipe for UsbLink {
    fn write_raw(&self, buf: &[u8]) -> Result<(), TransportError> {
        UsbLink::write_raw(self, buf)
    }
    fn read_raw(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
        UsbLink::read_raw(self, buf)
    }
}

/// Classic exchange: send `p`, consuming its ACK/NACK and answering foreign
/// packets. Returns the next sequence number. The caller owns every
/// address/service field; `host_sid` is the service id the device replies to.
fn classic_send(
    pipe: &impl Pipe,
    mut seq: u8,
    mut p: Packet,
    host_sid: u16,
) -> Result<u8, TransportError> {
    p.seq = seq;
    pipe.write_raw(&p.encode())?;

    // Wait for the matching ACK, NACKing anything else (e.g. LOGIN).
    let mut buf = vec![0u8; HEADER_SIZE + MAX_DATA_CLASSIC];
    loop {
        let n = pipe.read_raw(&mut buf)?;
        let r = Packet::decode(&buf[..n])
            .ok_or_else(|| TransportError::Protocol("bad packet from device".into()))?;
        if r.dst_sid == host_sid {
            if r.src_sid == protocol::SID_ACK || r.src_sid == protocol::SID_ACK0 {
                seq = seq.wrapping_add(1);
                if seq == 0 {
                    seq = 1;
                }
                return Ok(seq);
            }
            if r.src_sid == protocol::SID_NACK {
                return Err(TransportError::Protocol("device NACKed request".into()));
            }
        } else {
            // Not for us: ACK disconnects, NACK everything else.
            let reply = protocol::ack_for(&r, r.dst_sid != protocol::SID_DISCONNECT);
            let _ = pipe.write_raw(&reply.encode());
        }
    }
}

/// Classic receive: return the first packet addressed to the host, ACKing it.
fn classic_recv(pipe: &impl Pipe, maxlen: usize, host_sid: u16) -> Result<Packet, TransportError> {
    let mut buf = vec![0u8; HEADER_SIZE + MAX_DATA_CX2];
    loop {
        let n = pipe.read_raw(&mut buf)?;
        let mut r = Packet::decode(&buf[..n])
            .ok_or_else(|| TransportError::Protocol("bad packet from device".into()))?;
        if r.dst_sid == host_sid {
            let ack = protocol::ack_for(&r, false);
            pipe.write_raw(&ack.encode())?;
            r.data.truncate(maxlen.min(r.data.len()));
            return Ok(r);
        }
        let reply = protocol::ack_for(&r, r.dst_sid != protocol::SID_DISCONNECT);
        let _ = pipe.write_raw(&reply.encode());
    }
}

/// CX II transport: NavNet packets inside NNSE "stream" frames.
pub struct Cx2Link {
    usb: UsbLink,
    seqno: u16,
    ready: bool,
}

impl Cx2Link {
    fn new(usb: UsbLink) -> Self {
        Cx2Link { usb, seqno: 0, ready: false }
    }

    fn next_seq(&mut self) -> u16 {
        let s = self.seqno;
        self.seqno = self.seqno.wrapping_add(1);
        s
    }

    fn write_nnse(&self, msg: &[u8]) -> Result<(), TransportError> {
        self.usb.write_raw(msg)
    }

    fn read_nnse(&self, maxlen: usize) -> Result<Vec<u8>, TransportError> {
        let mut buf = vec![0u8; maxlen.max(HEADER_SIZE + MAX_DATA_CX2)];
        let n = self.usb.read_raw(&mut buf)?;
        buf.truncate(n);
        // Validate checksum if we have a length field.
        if buf.len() >= 16 {
            let len = u16::from_be_bytes([buf[12], buf[13]]) as usize;
            if len >= 16 && len <= n {
                if nnse_checksum(&buf[..len]) != 0xFFFF {
                    return Err(TransportError::Protocol("NNSE checksum mismatch".into()));
                }
                buf.truncate(len);
            }
        }
        Ok(buf)
    }

    fn build_nnse(service: u8, req_ack: u8, seqno: u16, data: &[u8]) -> Vec<u8> {
        let len = 16 + data.len();
        let mut m = vec![0u8; 16];
        m[0] = 0; // misc
        m[1] = service;
        m[2] = NNSE_ADDR_ME;
        m[3] = NNSE_ADDR_CALC;
        m[4] = 0; // unknown
        m[5] = req_ack;
        m[12..14].copy_from_slice(&(len as u16).to_be_bytes());
        m[14..16].copy_from_slice(&seqno.to_be_bytes());
        m.extend_from_slice(data);
        let ck = nnse_checksum(&m) ^ 0xFFFF;
        m[10..12].copy_from_slice(&ck.to_be_bytes());
        m
    }

    /// Run the NNSE handshake until the calculator is ready.
    fn assure_ready(&mut self) -> Result<(), TransportError> {
        if self.ready {
            return Ok(());
        }
        for _ in 0..10 {
            let msg = match self.read_nnse(16 + 1472) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if self.handle_msg(&msg) {
                self.ready = true;
                return Ok(());
            }
        }
        Err(TransportError::Protocol("CX II handshake did not complete".into()))
    }

    /// Answer a frame's request-for-ack with an NNSE ack, when it asked for one.
    fn ack_nnse(&mut self, msg: &[u8]) {
        if msg.len() >= 16 && msg[5] & 1 != 0 {
            let seqno = u16::from_be_bytes([msg[14], msg[15]]);
            let ack = Self::build_nnse(msg[1] | NNSE_ACK_FLAG, msg[5] & !1, seqno, &[]);
            let _ = self.write_nnse(&ack);
        }
    }

    /// Handle one NNSE control message. Returns true once the time response
    /// (handshake completion) has been sent.
    fn handle_msg(&mut self, msg: &[u8]) -> bool {
        if msg.len() < 16 {
            return false;
        }
        let service = msg[1];
        let src = msg[2];
        let dest = msg[3];
        let length = u16::from_be_bytes([msg[12], msg[13]]) as usize;
        if dest != NNSE_ADDR_ME && dest != 0xFF {
            return false;
        }
        if service & NNSE_ACK_FLAG != 0 {
            return false;
        }
        self.ack_nnse(msg);
        let data = &msg[16..length.clamp(16, msg.len())];
        match service & !NNSE_ACK_FLAG {
            NNSE_SVC_TIME => {
                // Time response: sec(u32) + frac(u64) + frac2(u32) after a 0x80 byte.
                let mut resp = Vec::with_capacity(1 + 16);
                resp.push(0x80);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                resp.extend_from_slice(&(now.as_secs() as u32).to_be_bytes());
                resp.extend_from_slice(&0u64.to_be_bytes());
                resp.extend_from_slice(&0u32.to_be_bytes());
                let m = Self::build_nnse(NNSE_SVC_TIME, 0, self.next_seq(), &resp);
                let _ = self.write_nnse(&m);
                return true;
            }
            NNSE_SVC_UNKNOWN if data.len() == 1 && data[0] == 0x01 => {
                let m = Self::build_nnse(NNSE_SVC_UNKNOWN, 0, self.next_seq(), &[0x81, 0x03]);
                let _ = self.write_nnse(&m);
            }
            _ => {}
        }
        let _ = src;
        false
    }

    /// Send a NavNet packet wrapped in an NNSE stream frame and wait for the ack.
    pub fn send_packet(&mut self, p: Packet) -> Result<(), TransportError> {
        self.assure_ready()?;
        let seqno = self.next_seq();
        let payload = p.encode();
        let msg = Self::build_nnse(NNSE_SVC_STREAM, 1, seqno, &payload);
        self.write_nnse(&msg)?;
        for _ in 0..10 {
            let r = match self.read_nnse(16 + 1472) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if r.len() >= 16
                && r[3] == NNSE_ADDR_ME
                && r[1] == NNSE_SVC_STREAM | NNSE_ACK_FLAG
                && u16::from_be_bytes([r[14], r[15]]) == seqno
            {
                return Ok(());
            }
            self.handle_msg(&r);
        }
        Err(TransportError::Protocol("no NNSE ack for stream packet".into()))
    }

    /// Receive a NavNet packet from an NNSE stream frame.
    pub fn recv_packet(&mut self, maxlen: usize) -> Result<Packet, TransportError> {
        self.assure_ready()?;
        for _ in 0..10 {
            let m = match self.read_nnse(16 + 1472) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if m.len() >= 16 && (m[1] & !NNSE_ACK_FLAG) == NNSE_SVC_STREAM {
                self.ack_nnse(&m);
                let length = u16::from_be_bytes([m[12], m[13]]) as usize;
                let data = &m[16..length.clamp(16, m.len())];
                if let Some(p) = Packet::decode(data) {
                    let mut p = p;
                    p.data.truncate(maxlen.min(p.data.len()));
                    // Ack at the NavNet layer too: as on the classic transport,
                    // the device waits for it before sending the next chunk.
                    let ack = protocol::ack_for(&p, false);
                    let msg = Self::build_nnse(NNSE_SVC_STREAM, 0, self.next_seq(), &ack.encode());
                    let _ = self.write_nnse(&msg);
                    return Ok(p);
                }
            }
            self.handle_msg(&m);
        }
        Err(TransportError::Protocol("no NNSE stream packet".into()))
    }
}

impl Link {
    pub fn send(&mut self, p: Packet, host_sid: u16) -> Result<(), TransportError> {
        match self {
            Link::Classic(u) => u.send_packet(p, host_sid),
            Link::Cx2(c) => c.send_packet(p),
        }
    }

    pub fn recv(&mut self, maxlen: usize, host_sid: u16) -> Result<Packet, TransportError> {
        match self {
            Link::Classic(u) => u.recv_packet(maxlen, host_sid),
            Link::Cx2(c) => c.recv_packet(maxlen),
        }
    }

    /// Run the transport handshake. Returns the initial host service id the
    /// caller should use for its first packet.
    pub fn init(&mut self) -> Result<(), TransportError> {
        match self {
            Link::Classic(u) => u.classic_init(),
            Link::Cx2(c) => c.assure_ready(),
        }
    }
}

fn trace_enabled() -> bool {
    std::env::var_os("TI_NSPIRE_TRACE").is_some()
}

fn hex(buf: &[u8]) -> String {
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// NNSE 16-bit ones-complement checksum (big-endian 16-bit words).
fn nnse_checksum(data: &[u8]) -> u16 {
    let mut acc: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        acc += ((data[i] as u32) << 8) | data[i + 1] as u32;
        i += 2;
    }
    if i < data.len() {
        acc += (data[i] as u32) << 8;
    }
    while acc >> 16 != 0 {
        acc = (acc >> 16) + (acc & 0xFFFF);
    }
    acc as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted in-memory pipe: queued reads, recorded writes, no USB.
    struct FakePipe {
        reads: std::cell::RefCell<std::collections::VecDeque<Vec<u8>>>,
        writes: std::cell::RefCell<Vec<Vec<u8>>>,
    }

    impl FakePipe {
        fn new(reads: Vec<Vec<u8>>) -> Self {
            FakePipe {
                reads: std::cell::RefCell::new(reads.into()),
                writes: std::cell::RefCell::new(Vec::new()),
            }
        }
    }

    impl Pipe for FakePipe {
        fn write_raw(&self, buf: &[u8]) -> Result<(), TransportError> {
            self.writes.borrow_mut().push(buf.to_vec());
            Ok(())
        }
        fn read_raw(&self, buf: &mut [u8]) -> Result<usize, TransportError> {
            let chunk = self
                .reads
                .borrow_mut()
                .pop_front()
                .ok_or(TransportError::NoDevice)?;
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            Ok(n)
        }
    }

    fn ack_from_device(src_sid: u16, seq: u8) -> Packet {
        Packet::new(protocol::DEVICE_ADDR, src_sid, protocol::HOST_ADDR, protocol::HOST_SID, seq)
    }

    #[test]
    fn classic_send_advances_seq_on_ack() {
        let pipe = FakePipe::new(vec![ack_from_device(protocol::SID_ACK, 1).encode()]);
        let req = Packet::new(protocol::HOST_ADDR, protocol::HOST_SID, protocol::DEVICE_ADDR, protocol::SID_FILE, 0);
        let next = classic_send(&pipe, 1, req, protocol::HOST_SID).unwrap();
        assert_eq!(next, 2);
        assert_eq!(pipe.writes.borrow().len(), 1); // just the request
    }

    #[test]
    fn classic_send_fails_on_nack() {
        let pipe = FakePipe::new(vec![ack_from_device(protocol::SID_NACK, 1).encode()]);
        let req = Packet::new(protocol::HOST_ADDR, protocol::HOST_SID, protocol::DEVICE_ADDR, protocol::SID_FILE, 0);
        assert!(classic_send(&pipe, 1, req, protocol::HOST_SID).is_err());
    }

    #[test]
    fn classic_send_nacks_foreign_then_accepts_ack() {
        // A packet for someone else is NACKed; the following ACK is consumed.
        let foreign = Packet::new(protocol::DEVICE_ADDR, 0x4003, protocol::HOST_ADDR, 0x9999, 0);
        let pipe = FakePipe::new(vec![
            foreign.encode(),
            ack_from_device(protocol::SID_ACK, 5).encode(),
        ]);
        let req = Packet::new(protocol::HOST_ADDR, protocol::HOST_SID, protocol::DEVICE_ADDR, protocol::SID_FILE, 0);
        let next = classic_send(&pipe, 5, req, protocol::HOST_SID).unwrap();
        assert_eq!(next, 6);
        let writes = pipe.writes.borrow();
        assert_eq!(writes.len(), 2); // request + NACK reply
        assert_eq!(Packet::decode(&writes[1]).unwrap().src_sid, protocol::SID_NACK);
    }

    #[test]
    fn classic_recv_acks_and_returns_data() {
        let data = Packet {
            src_addr: protocol::DEVICE_ADDR,
            src_sid: protocol::SID_FILE,
            dst_addr: protocol::HOST_ADDR,
            dst_sid: protocol::HOST_SID,
            ack: 0,
            seq: 7,
            data: vec![1, 2, 3],
        };
        let pipe = FakePipe::new(vec![data.encode()]);
        let got = classic_recv(&pipe, 254, protocol::HOST_SID).unwrap();
        assert_eq!(got.data, vec![1, 2, 3]);
        assert_eq!(pipe.writes.borrow().len(), 1); // the ACK
    }

    #[test]
    fn nnse_checksum_self_inverse() {
        // A checksum field chosen as ~sum makes the whole buffer sum to 0xFFFF.
        let mut m = vec![0u8; 16];
        m[12] = 0;
        m[13] = 16;
        let ck = nnse_checksum(&m) ^ 0xFFFF;
        m[10..12].copy_from_slice(&ck.to_be_bytes());
        assert_eq!(nnse_checksum(&m), 0xFFFF);
    }
}
