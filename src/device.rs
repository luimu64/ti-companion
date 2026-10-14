//! High-level TI-Nspire file operations built on the NavNet protocol.
//!
//! Command layout recovered from `navnet.dll` (TI-Nspire Student Software
//! 5.4.0.259) — the `*_CMD` string table with its numeric dispatch — and
//! cross-checked against the wire formats.
//!
//! File-manager service `0x4060`, first data byte is the command:
//! ```text
//! 0x03  put file      (0x03 0x01 path size:u32) then 0x05 chunks
//! 0x04  ok to send/receive (also the "done" marker of a download)
//! 0x05  file data chunk
//! 0x07  get file      (0x07 0x01 path)
//! 0x09  delete file   (0x09 0x01 path)
//! 0x0A  create folder (0x0A 0x03 path)
//! 0x0B  delete folder (0x0B 0x03 path)
//! 0x0C  copy          (0x0C 0x01 src dst)
//! 0x0D  dirlist init  (0x0D path 0)
//! 0x0E  dirlist next
//! 0x0F  dirlist done
//! 0x10  dirlist entry
//! 0x20  file attributes (0x20 0x01 path)
//! 0x21  rename/move   (0x21 0x01 src dst)
//! ```
//! Strings are NUL-terminated and zero-padded so that they occupy at least
//! 8 bytes plus the terminator.

use crate::protocol::*;
use crate::transport::{Link, TransportError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirItem {
    pub name: String,
    pub size: u32,
    pub date: u32,
    pub is_dir: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevInfo {
    pub flash_free: u64,
    pub flash_total: u64,
    pub ram_free: u64,
    pub ram_total: u64,
    pub os_version: (u8, u8, u16),
    pub boot1_version: (u8, u8, u16),
    pub boot2_version: (u8, u8, u16),
    pub device_type: u8,
    pub battery: u8,
    pub clock_speed: u8,
    pub lcd_width: u16,
    pub lcd_height: u16,
    pub device_name: String,
    pub runlevel: u16,
}

type R<T> = Result<T, TransportError>;

/// A connected, initialised calculator.
pub struct Calc {
    link: Link,
    host_sid: u16,
    connected: bool,
    device_sid: u16,
}

impl Calc {
    pub fn connect() -> R<Calc> {
        let mut link = Link::open()?;
        link.init()?;
        Ok(Calc { link, host_sid: HOST_SID, connected: false, device_sid: INIT_SID })
    }

    pub fn is_cx2(&self) -> bool {
        self.link.is_cx2()
    }

    fn max_data(&self) -> usize {
        self.link.max_data()
    }

    fn packet(&self) -> Packet {
        Packet::new(HOST_ADDR, self.host_sid, DEVICE_ADDR, self.device_sid, 0)
    }

    /// Open a service: subsequent writes go to `sid`.
    fn service_connect(&mut self, sid: u16) -> R<()> {
        self.connected = true;
        self.device_sid = sid;
        Ok(())
    }

    /// Close the service and bump the host service id, as the device expects.
    fn service_disconnect(&mut self) -> R<()> {
        if !self.connected {
            return Ok(());
        }
        let mut p = self.packet();
        p.src_sid = SID_DISCONNECT;
        p.data = self.host_sid.to_be_bytes().to_vec();
        self.send_sync(&mut p)?;
        self.connected = false;
        self.host_sid = self.host_sid.wrapping_add(1);
        Ok(())
    }

    /// Send a packet; the transport owns the sequence number and ACK handling.
    fn send_sync(&mut self, p: &mut Packet) -> R<()> {
        p.src_addr = HOST_ADDR;
        p.src_sid = if p.src_sid == 0 { self.host_sid } else { p.src_sid };
        p.dst_addr = DEVICE_ADDR;
        p.dst_sid = self.device_sid;
        let pkt = p.clone();
        self.link.send(pkt, self.host_sid)
    }

    /// Write a data payload to the device, splitting if needed.
    fn data_write(&mut self, data: &[u8]) -> R<()> {
        let max = self.max_data();
        if data.len() <= max {
            let mut p = self.packet();
            p.data = data.to_vec();
            self.send_sync(&mut p)
        } else {
            for chunk in data.chunks(max) {
                let mut p = self.packet();
                p.data = chunk.to_vec();
                self.send_sync(&mut p)?;
            }
            Ok(())
        }
    }

    fn data_read(&mut self, maxlen: usize) -> R<Vec<u8>> {
        let p = self.link.recv(maxlen, self.host_sid)?;
        Ok(p.data)
    }

    // ---- file-manager operations -------------------------------------------

    /// List the direct children of `path`.
    pub fn dirlist(&mut self, path: &str) -> R<Vec<DirItem>> {
        self.service_connect(SID_FILE)?;
        let result = self.dirlist_inner(path);
        let _ = self.service_disconnect();
        result
    }

    fn dirlist_inner(&mut self, path: &str) -> R<Vec<DirItem>> {
        // 0x0D path 0
        let mut init = vec![0x0D];
        push_padded_string(&mut init, path);
        init.push(0);
        self.data_write(&init)?;
        let r = self.data_read(2)?;
        match r.first().copied() {
            Some(0x0A) => return Err(TransportError::Protocol("path does not exist".into())),
            Some(0x0F) => return Err(TransportError::Protocol("invalid directory".into())),
            _ => {}
        }

        let mut items = Vec::new();
        loop {
            let mut next = self.packet();
            next.data = vec![0x0E];
            self.send_sync(&mut next)?;
            let entry = self.data_read(254)?;
            if entry.first() == Some(&0xFF) {
                break;
            }
            if entry.len() < 10 {
                return Err(TransportError::Protocol("short dirlist entry".into()));
            }
            items.push(parse_dirlist_entry(&entry)?);
        }

        let mut done = self.packet();
        done.data = vec![0x0F];
        self.send_sync(&mut done)?;
        let _ = self.data_read(2)?;
        Ok(items)
    }

    /// Create a folder.
    pub fn mkdir(&mut self, path: &str) -> R<()> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x0A, 0x03];
        push_padded_string(&mut d, path);
        let res = self.simple_command(&d);
        let _ = self.service_disconnect();
        res
    }

    /// Remove an empty folder.
    pub fn rmdir(&mut self, path: &str) -> R<()> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x0B, 0x03];
        push_padded_string(&mut d, path);
        let res = self.simple_command(&d);
        let _ = self.service_disconnect();
        res
    }

    /// Delete a file.
    pub fn delete_file(&mut self, path: &str) -> R<()> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x09, 0x01];
        push_padded_string(&mut d, path);
        d.push(0);
        let res = self.simple_command(&d);
        let _ = self.service_disconnect();
        res
    }

    /// Rename or move a file/folder.
    pub fn rename(&mut self, src: &str, dst: &str) -> R<()> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x21, 0x01];
        push_padded_string(&mut d, src);
        push_padded_string(&mut d, dst);
        d.push(0);
        let res = self.simple_command(&d);
        let _ = self.service_disconnect();
        res
    }

    /// Copy a file/folder.
    pub fn copy(&mut self, src: &str, dst: &str) -> R<()> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x0C, 0x01];
        push_padded_string(&mut d, src);
        push_padded_string(&mut d, dst);
        d.push(0);
        let res = self.simple_command(&d);
        let _ = self.service_disconnect();
        res
    }

    /// Fetch attributes for a path.
    pub fn attributes(&mut self, path: &str) -> R<DirItem> {
        self.service_connect(SID_FILE)?;
        let mut d = vec![0x20, 0x01];
        push_padded_string(&mut d, path);
        d.push(0);
        let result = (|| -> R<DirItem> {
            self.data_write(&d)?;
            let buf = self.data_read(254)?;
            if buf.first() != Some(&0x20) {
                return Err(TransportError::Protocol("path does not exist".into()));
            }
            if buf.len() < 11 {
                return Err(TransportError::Protocol("short attribute response".into()));
            }
            let size = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]);
            let date = u32::from_be_bytes([buf[5], buf[6], buf[7], buf[8]]);
            let is_dir = buf[9] != 0;
            Ok(DirItem { name: path.to_string(), size, date, is_dir })
        })();
        let _ = self.service_disconnect();
        result
    }

    /// Download a file from the calculator, reporting `(received, total)` after
    /// each chunk.
    pub fn download_with_progress(
        &mut self,
        path: &str,
        progress: &mut dyn FnMut(usize, usize),
    ) -> R<Vec<u8>> {
        self.service_connect(SID_FILE)?;
        let result = self.download_inner(path, progress);
        let _ = self.service_disconnect();
        result
    }

    fn download_inner(
        &mut self,
        path: &str,
        progress: &mut dyn FnMut(usize, usize),
    ) -> R<Vec<u8>> {
        let mut d = vec![0x07, 0x01];
        push_padded_string(&mut d, path);
        self.data_write(&d)?;
        let header = self.data_read(254)?;
        // header: result:u16, then 9 bytes, then data_len:u32 at offset 11
        if header.len() < 15 {
            return Err(TransportError::Protocol("short get-file response".into()));
        }
        let result = u16::from_be_bytes([header[0], header[1]]);
        if result != 0x0301 {
            return Err(TransportError::Protocol("file does not exist".into()));
        }
        let mut remaining =
            u32::from_be_bytes([header[11], header[12], header[13], header[14]]) as usize;

        // Tell the device we are ready to receive.
        let mut go = self.packet();
        go.data = vec![0x04];
        self.send_sync(&mut go)?;

        let chunk = self.max_data() - 1;
        let total = remaining;
        let mut out = Vec::with_capacity(remaining);
        while remaining > 0 {
            let want = chunk.min(remaining);
            let data = self.link.recv(want + 1, self.host_sid)?;
            if data.data.is_empty() {
                return Err(TransportError::Protocol("empty data chunk".into()));
            }
            let payload = &data.data[1..];
            out.extend_from_slice(payload);
            remaining -= payload.len();
            progress(total - remaining, total);
        }

        // Acknowledge completion.
        let mut fin = self.packet();
        fin.data = 0xFF00u16.to_be_bytes().to_vec();
        self.send_sync(&mut fin)?;
        Ok(out)
    }

    /// Upload a file to the calculator, reporting `(sent, total)` after each
    /// chunk.
    pub fn upload_with_progress(
        &mut self,
        path: &str,
        data: &[u8],
        progress: &mut dyn FnMut(usize, usize),
    ) -> R<()> {
        self.service_connect(SID_FILE)?;
        let result = self.upload_inner(path, data, progress);
        let _ = self.service_disconnect();
        result
    }

    fn upload_inner(
        &mut self,
        path: &str,
        data: &[u8],
        progress: &mut dyn FnMut(usize, usize),
    ) -> R<()> {
        // 0x03 0x01 path size:u32
        let mut d = vec![0x03, 0x01];
        push_padded_string(&mut d, path);
        d.extend_from_slice(&(data.len() as u32).to_be_bytes());
        self.data_write(&d)?;
        let resp = self.data_read(254)?;
        // Two shapes are possible:
        //  * a single byte 0x04  -> "send the data"
        //  * two bytes 0xFF <code> -> an immediate status (e.g. bad extension)
        match resp.as_slice() {
            [0x04] => {}
            [0xFF, code] => {
                return Err(TransportError::Protocol(format!(
                    "device refused upload: {}",
                    status_text(*code)
                )));
            }
            other => {
                return Err(TransportError::Protocol(format!(
                    "unexpected upload response {other:02x?}"
                )));
            }
        }

        let chunk = self.max_data() - 1;
        let total = data.len();
        let mut sent = 0;
        for part in data.chunks(chunk) {
            let mut p = self.packet();
            p.data = Vec::with_capacity(part.len() + 1);
            p.data.push(0x05);
            p.data.extend_from_slice(part);
            self.send_sync(&mut p)?;
            sent += part.len();
            progress(sent, total);
        }

        let resp = self.data_read(254)?;
        match resp.as_slice() {
            [0xFF, 0x00] => Ok(()),
            [0xFF, code] => Err(TransportError::Protocol(format!(
                "upload failed: {}",
                status_text(*code)
            ))),
            other => Err(TransportError::Protocol(format!(
                "unexpected upload result {other:02x?}"
            ))),
        }
    }

    /// Send a short command and check the 2-byte `FF <status>` reply.
    fn simple_command(&mut self, data: &[u8]) -> R<()> {
        self.data_write(data)?;
        let resp = self.data_read(2)?;
        match resp.as_slice() {
            [0xFF, 0x00] => Ok(()),
            [0xFF, code] => Err(TransportError::Protocol(format!(
                "command failed: {}",
                status_text(*code)
            ))),
            // Some firmware variants echo the raw command word on success.
            other => Err(TransportError::Protocol(format!(
                "unexpected reply {other:02x?}"
            ))),
        }
    }

    // ---- device information ------------------------------------------------

    pub fn device_info(&mut self) -> R<DevInfo> {
        self.service_connect(SID_DEVINFO)?;
        let result = self.device_info_inner();
        let _ = self.service_disconnect();
        result
    }

    fn device_info_inner(&mut self) -> R<DevInfo> {
        let mut p = self.packet();
        p.data = vec![0x01];
        self.send_sync(&mut p)?;
        let buf = self.data_read(253)?;
        // Response is shifted by one leading byte.
        let d = &buf[1.min(buf.len())..];
        if d.len() < 65 {
            return Err(TransportError::Protocol("short device info".into()));
        }
        let u64at = |o: usize| u64::from_be_bytes(d[o..o + 8].try_into().unwrap());
        let ver = |o: usize| (d[o], d[o + 1], u16::from_be_bytes([d[o + 2], d[o + 3]]));
        let info = DevInfo {
            flash_free: u64at(0),
            flash_total: u64at(8),
            ram_free: u64at(16),
            ram_total: u64at(24),
            battery: d[32],
            // d[33] padding, d[34] is_charging
            clock_speed: d[35],
            os_version: ver(36),
            boot1_version: ver(40),
            boot2_version: ver(44),
            runlevel: u16::from_be_bytes([d[52], d[53]]),
            lcd_width: u16::from_be_bytes([d[58], d[59]]),
            lcd_height: u16::from_be_bytes([d[60], d[61]]),
            device_type: d[64],
            device_name: String::new(),
        };

        // Device name.
        let mut p = self.packet();
        p.data = vec![0x02];
        self.send_sync(&mut p)?;
        let buf = self.data_read(253)?;
        let name = cstr_from(&buf[1.min(buf.len())..]).unwrap_or_default();

        // Extensions (file, os) — we only keep the info for completeness.
        let mut p = self.packet();
        p.data = vec![0x03];
        self.send_sync(&mut p)?;
        let _ = self.data_read(253)?;

        Ok(DevInfo { device_name: name, ..info })
    }
}

/// Parse one `0x10` dirlist entry.
/// Layout: `0x10`, entry-len:u16, padded(name), size:u32, date:u32, is_dir:u8, 0.
fn parse_dirlist_entry(entry: &[u8]) -> R<DirItem> {
    if entry.len() < 3 || entry[0] != 0x10 {
        return Err(TransportError::Protocol("unexpected dirlist entry".into()));
    }
    let entry_len = u16::from_be_bytes([entry[1], entry[2]]) as usize;
    let (name, off) = read_padded_string(entry, 3)?;
    // The declared length must at least cover the name and the trailing fields.
    if entry_len < off + 9 || off + 9 > entry.len() {
        return Err(TransportError::Protocol("truncated dirlist entry".into()));
    }
    let size = u32::from_be_bytes([entry[off], entry[off + 1], entry[off + 2], entry[off + 3]]);
    let date = u32::from_be_bytes([entry[off + 4], entry[off + 5], entry[off + 6], entry[off + 7]]);
    let is_dir = entry[off + 8] != 0;
    Ok(DirItem { name, size, date, is_dir })
}

/// Append a NUL-terminated string padded so it occupies at least 8 bytes.
fn push_padded_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(bytes);
    if bytes.len() < 8 {
        out.resize(out.len() + (8 - bytes.len()), 0);
    }
    out.push(0);
}

/// Read a NUL-terminated, min-8-byte-padded string; returns (string, next offset).
/// Rejects a name that is not valid UTF-8 rather than replacing its bytes, so a
/// later operation cannot target the wrong file.
fn read_padded_string(buf: &[u8], start: usize) -> R<(String, usize)> {
    let nul = buf[start..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| p + start)
        .ok_or_else(|| TransportError::Protocol("bad dirlist name".into()))?;
    let name = std::str::from_utf8(&buf[start..nul])
        .map_err(|_| TransportError::Protocol("non-UTF-8 file name".into()))?
        .to_string();
    let consumed = nul - start;
    let next = if consumed < 8 { start + 8 + 1 } else { nul + 1 };
    Ok((name, next))
}

fn cstr_from(buf: &[u8]) -> Option<String> {
    let nul = buf.iter().position(|&b| b == 0)?;
    std::str::from_utf8(&buf[..nul]).ok().map(str::to_string)
}

/// Map a device status byte to the 16-bit error code the TI software uses.
/// Table transcribed from navnet.dll at VA 0x18004ec00 (indexed by status byte).
const STATUS_TO_ERROR: [u16; 25] = [
    0x0001, 0xFFFF, 0xFFFE, 0xFFFD, 0xFFFC, 0xFFFB, 0xFF00, 0xFEEC, 0xFEF2, 0xFEED, 0xFEF8,
    0xFEEA, 0xFEEB, 0xFEF6, 0xFEF3, 0xFEEF, 0xFEF9, 0xFEEE, 0xFEE9, 0xFEE6, 0xFEE5, 0xFEE4,
    0xFEE3, 0xFEF0, 0xFEE8,
];

/// Human-readable text for a device status byte.
fn status_text(code: u8) -> String {
    if code == 0 {
        return "ok".into();
    }
    let err = STATUS_TO_ERROR.get(code as usize).copied();
    let name = match err {
        Some(0xFEE3) => "illegal character in name",
        Some(0xFEE4) | Some(0xFEF2) => "unsupported file type",
        Some(0xFEE5) => "folder depth not supported",
        Some(0xFEE6) => "path too long",
        Some(0xFEE9) => "host I/O error",
        Some(0xFEEA) | Some(0xFEEB) => "permission denied",
        Some(0xFEEC) | Some(0xFEED) => "bad path",
        Some(0xFEEF) => "protocol error",
        Some(0xFEF3) => "device busy",
        Some(0xFEF6) => "operation cancelled",
        Some(0xFEF8) => "file not found",
        Some(0xFFFF) => "storage full",
        _ => "device error",
    };
    match err {
        Some(e) => format!("{name} (status {code:#04x}, code {e:#06x})"),
        None => format!("{name} (status {code:#04x})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padded_string_roundtrip() {
        let mut v = vec![0x0D];
        push_padded_string(&mut v, "/");
        v.push(0);
        // 0x0D + padded("/") (8 bytes) + NUL + extra NUL, as data_build("bs0", ...).
        assert_eq!(v, vec![0x0D, b'/', 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let (s, next) = read_padded_string(&v, 1).unwrap();
        assert_eq!(s, "/");
        // 8-byte padded field plus its terminator: next points past index 9.
        assert_eq!(next, 10);
    }

    #[test]
    fn dirlist_entry_parses() {
        // "Foo" padded to 8 + NUL, size 0x11223344, date 0x5555, file.
        let mut e = vec![0x10, 0x00, 0x15];
        e.extend_from_slice(b"Foo\0\0\0\0\0\0");
        e.extend_from_slice(&0x1122_3344u32.to_be_bytes());
        e.extend_from_slice(&0x0000_5555u32.to_be_bytes());
        e.push(0x00);
        e.push(0x00);
        let item = parse_dirlist_entry(&e).unwrap();
        assert_eq!(item.name, "Foo");
        assert_eq!(item.size, 0x1122_3344);
        assert_eq!(item.date, 0x0000_5555);
        assert!(!item.is_dir);
    }

    #[test]
    fn dirlist_entry_rejects_bad_length_and_utf8() {
        // entry-len says 5, too short to hold the name and trailing fields.
        let mut e = vec![0x10, 0x00, 0x05];
        e.extend_from_slice(b"Foo\0\0\0\0\0\0");
        e.extend_from_slice(&[0u8; 9]);
        assert!(parse_dirlist_entry(&e).is_err());

        // A name carrying a non-UTF-8 byte is rejected, not lossily replaced.
        let mut e = vec![0x10, 0x00, 0x15];
        e.extend_from_slice(b"F\xffo\0\0\0\0\0");
        e.extend_from_slice(&[0u8; 9]);
        assert!(parse_dirlist_entry(&e).is_err());
    }

    #[test]
    fn long_padded_string() {
        let mut v = vec![];
        push_padded_string(&mut v, "/a/verylongpath.doc");
        v.push(0);
        let (s, next) = read_padded_string(&v, 0).unwrap();
        assert_eq!(s, "/a/verylongpath.doc");
        assert_eq!(next, v.len() - 1);
    }
}
