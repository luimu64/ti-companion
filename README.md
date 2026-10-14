# TI-Nspire Companion

A small cross-platform (Linux-first) GUI file manager for **physical TI-Nspire
calculators** connected over USB. It reimplements the calculator's USB link
protocol — the NavNet link protocol — from scratch in Rust.

Scope: everything related to *file operations on the physical calculator*:

* browse the directory tree like a normal file manager — double-click a folder
  to enter it, double-click a file to download it, `Enter` opens the selection,
  `Backspace` goes up a level, and the breadcrumb path jumps to any ancestor
* multi-select with `Ctrl`-click (toggle) and `Shift`-click (range), `Ctrl+A`
  selects all; bulk delete acts on the whole selection
* upload files (any local file; see the naming note below)
* download files, with a progress indicator
* rename / move
* copy / duplicate
* delete files or folders (single or in bulk), confirmed in a modal first
* create and delete folders
* read file attributes and device information

The listing refreshes automatically after any action that changes it (upload,
rename, copy, delete, new folder).

**File naming.** The calculator only stores documents whose name ends in
`.tns` (lowercase); it does not inspect the contents. When you upload anything
else, the GUI prompts and uploads it as `<name>.<ext>.tns` — the original
extension is kept in front of the forced `.tns` — so e.g. `report.pdf` becomes
`report.pdf.tns`. Files already ending in `.tns` are uploaded unchanged.
Anything the device still refuses is reported as "unsupported file type".

Emulator/document features of the official Student Software are intentionally
out of scope.

## Building and running

```sh
cargo build --release
./target/release/ti-companion
```

Build requirements: a Rust toolchain and a C compiler plus libusb headers
(`libusb-1.0-0-dev` on Debian/Ubuntu, `libusb-devel` on Fedora,
`libusb` on Arch/macOS via Homebrew).

At runtime on Linux, the file pickers go through `rfd`'s default XDG Desktop
Portal backend, so a portal implementation must be running — GNOME, KDE or
`xdg-desktop-portal-gtk`. The wlroots portal backend does not implement the
D-Bus API `rfd` needs. Without a suitable portal the pickers are silently
reported as cancelled, so choosing a file to upload or download appears to do
nothing. (`zenity` is not used.)

### Prebuilt binaries

Every commit on `master` is built and published to the rolling
[`latest` release](../../releases/tag/latest) — an x86_64 AppImage for Linux and
a standalone x86_64 `.exe` for Windows. That tag always points at the newest
build, so this URL is stable:

```sh
# The asset filenames include the build date and commit, so list before fetching:
curl -fsS https://api.github.com/repos/luimu64/ti-companion/releases/tags/latest \
  | grep browser_download_url
```

Or just open the [releases page](../../releases) and download the asset. The
rolling tag is `latest`, so
`https://github.com/luimu64/ti-companion/releases/tag/latest` always shows the
newest build.

The AppImage bundles libusb, so no system packages are required beyond the usual
desktop graphics stack. On Windows, run the `.exe` directly; it links libusb
statically.

Each commit also keeps per-commit artifacts
(`TI-Nspire-Companion-<sha>-x86_64.AppImage` and `…-x86_64.exe`) on its workflow
run for 90 days; those download as a zip.

Build the AppImage yourself with [`appimagetool`](https://github.com/AppImage/appimagetool):

```sh
cargo build --release --locked
# assemble AppDir per .github/workflows/build.yml, then:
ARCH=x86_64 appimagetool AppDir
```

If the calculator does not appear or an operation hangs, run the headless
diagnostic, which prints each USB step with timings:

```sh
./target/release/ti-companion --probe
TI_NSPIRE_TRACE=1 ./target/release/ti-companion --probe   # dump every packet
```

## MCP server (agent control)

The app ships an MCP server so an agent can drive the calculator. It talks to the
same calculator worker as the GUI, so the two never race for the USB device.

Run it standalone (stdio transport):

```sh
./target/release/ti-companion --mcp
```

Or run the GUI and expose MCP at the same time:

```sh
TI_NSPIRE_MCP=1 ./target/release/ti-companion
```

Example client configuration (e.g. Claude Desktop, any MCP client):

```json
{
  "mcpServers": {
    "ti-nspire": {
      "command": "/path/to/ti-companion",
      "args": ["--mcp"]
    }
  }
}
```

Tools (all operate on the physical calculator):

| Tool | Purpose |
| --- | --- |
| `connect` | open the USB link and handshake |
| `device_info` | model, OS/boot versions, storage, RAM |
| `list_directory` | list a directory (JSON) |
| `file_info` | size/type/date for a path |
| `upload_file` | copy a local file to the calculator |
| `download_file` | copy a calculator file to a local path |
| `read_file` | read a calculator file, returned as base64 |
| `write_file` | write base64 data to a calculator file |
| `create_folder` | make a folder |
| `rename` | rename or move |
| `copy` | copy a file or folder |
| `delete` | delete a file or (empty) folder |

Note: the calculator only accepts documents whose name ends in `.tns`. Unlike
the GUI, the MCP tools do **not** rename for you — pass a `.tns` path, or the
device reports "unsupported file type".

On Linux, install the udev rule so the calculator is accessible without root:

```sh
sudo install -m 644 99-ti-nspire.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

The app finds the calculator by USB vendor/product id, so it works on any
platform with libusb (Linux, macOS, Windows).

## How the protocol was recovered

The starting artifact was the Windows installer
`TINspireCXCASStudentSoftware-5.4.0.259.exe`, which unpacks to a Java
application plus native link libraries. Mapping the shipped modules showed:

| Module | Role |
| --- | --- |
| `lib/docfw.jar` | Phoenix editor/engine (out of scope) |
| `lib/commproxy.jar` | transport-agnostic file API (`ICommProxy`) |
| `lib/connectors/nwb_nspire_connector.dll` | Nspire USB transport over WinUSB |
| `lib/navnet.dll` | NavNet link protocol: packets, commands, file manager |
| `bridgelink.dll` | Navigator IPC only (not the calculator link) |
| `GoIO_DLL.dll` | Vernier sensor library (not the calculator link) |
| `drivers/Nspire*/…inf` | VID 0x0451, PID 0xE012 (classic/CX), 0xE022 (CX II, WinUSB) |

`navnet.dll` was analysed with Ghidra (through REA) as an x86-64 PE. The string
table revealed the command names (`DIR_ENUM_INIT_CMD`, `RQST_TO_SEND_VAR_CMD`,
`DELETE_VAR_CMD`, `MKDIR_CMD`, `RMDIR_CMD`, `COPY_CMD`, `RENAME_CMD`,
`FILE_ATTR_CMD`, `DEVICE_INFO_CMD`, …). The function that maps a command byte to
its name — the jump table at `FUN_1800139b0` — gives the exact numeric values:

```text
0x02 PROCESS_QUEUE_CMD      0x0D DIR_ENUM_INIT_CMD    0x20 FILE_ATTR_CMD
0x03 RQST_TO_SEND_VAR_CMD   0x0E DIR_ENUM_NEXT_CMD    0x21 RENAME_CMD
0x06 EMPTY_QUEUE_CMD        0x0F DIR_ENUM_DONE_CMD
0x07 RQST_FOR_VAR_CMD       0x09 DELETE_VAR_CMD
                            0x0A MKDIR_CMD
                            0x0B RMDIR_CMD
                            0x0C COPY_CMD
```

The packet framing came from `TI_NS_ConstructPacket` (`FUN_18001ca00`) and its
allocator `FUN_18001c8e0`:

* magic stored as the little-endian constant `0xfd54` → bytes `54 FD`
* two 32-bit "addresses" written big-endian at offsets 2 and 6
* a 16-bit payload hash at offset 10
* a one-byte length at offset 12 (`0xFF` ⇒ 32-bit length at offset 16..20)
* a one-byte header checksum at offset 15, computed as the sum of the first 15
  bytes (`FUN_18001c810` recomputes and compares it)

This is byte-for-byte the header documented by the community for the TI-Nspire
link protocol and implemented by `libnspire`, which confirms the reversal.

## The protocol

### Packet framing (16-byte header, all multi-byte fields big-endian)

```text
off  size  field
0    2     magic 0x54FD
2    2     source address
4    2     source service id (sid)
6    2     destination address
8    2     destination service id (sid)
10   2     data checksum
12   1     data size (0xFF ⇒ extended: u32 BE size at 16..20, data at 20)
13   1     ack flag (0x0A on ACK packets, else 0)
14   1     sequence number
15   1     header checksum = (sum of bytes 0..=14) mod 256
16   ...   data
```

Payload sizes: up to 254 bytes for classic Nspire/CX, up to 1440 for CX II.

The data checksum is a byte-wise mix:

```text
cs = 0
for b in data:
    tmp1 = (b << 8) | (cs >> 8)
    tmp2 = (((cs & 0xF) << 4) ^ cs) << 8
    tmp3 = tmp2 >> 5
    cs = (tmp3 >> 7) ^ tmp1 ^ tmp2 ^ tmp3
```

### Handshake (classic / CX)

```text
host addr   = 0x6400     device addr = 0x6401
initial sid = 0x4003     host sid after assign = 0x8000
seq starts at 1
```

1. Wait for the calculator's *address request* packet.
2. Send a packet with data `64 01 FF 00`.
3. Host service id becomes `0x8000`.

### Acknowledgements

The calculator ACKs each host packet with a packet whose source sid is `0x00FF`
(`0x00FE` when the acked sequence number was 0). The host increments its
sequence number on every ACK. Packets not addressed to the host are ACKed if
they are disconnects (`src sid 0x40DE`) and NACKed (`src sid 0x00D3`)
otherwise.

### Service (connection) model

A file operation is bracketed by *connect* and *disconnect* on a service id:

* `0x4060` — file manager
* `0x4020` — device information

Disconnect sends a two-byte payload equal to the host sid, with the packet's
source sid forced to `0x40DE`; the host sid is then incremented.

### CX II transport (NNSE)

The CX II wraps each NavNet packet in a *NavNet SE* frame: a 16-byte header
(misc, service, src, dest, unknown, reqAck, length u16, seqno u16, ones-complement
checksum u16) and a payload. Service `0x04` carries stream data (the NavNet
packet). The handshake is driven by the calculator's address-request and time
services. See `transport.rs::Cx2Link`.

### File-manager commands (service 0x4060)

| Request | Payload | Reply |
| --- | --- | --- |
| dirlist init | `0D` + padded(path) + `00` | `0A` = not found, `0F` = invalid |
| dirlist next | `0E` | entry, or `FF` when done |
| dirlist entry | — | `10` + padded(name) + size:u32 + date:u32 + is_dir:u8 + `00` |
| dirlist done | `0F` | result:u16 |
| create folder | `0A 03` + padded(path) | result:u16 (`FF00` = ok) |
| delete folder | `0B 03` + padded(path) | result:u16 |
| delete file | `09 01` + padded(path) + `00` | result:u16 |
| rename/move | `21 01` + padded(src) + padded(dst) + `00` | result:u16 |
| copy | `0C 01` + padded(src) + padded(dst) + `00` | result:u16 |
| attributes | `20 01` + padded(path) + `00` | `20` + size:u32 + date:u32 + is_dir:u8 + `00` |
| upload | `03 01` + padded(path) + size:u32 | `04` = go, then data chunks, final result:u16 |
| upload data | `05` + up to (max-1) bytes | — |
| download | `07 01` + padded(path) | result:u16 (`0301`), … , length:u32 |
| download go | `04` | data chunks: `05` + payload |
| download done | `FF00` | — |

Strings are NUL-terminated and zero-padded so they occupy at least 8 bytes
before the terminator.

Device information (service 0x4020): command `01` returns the packed device
info structure, `02` the device name, `03` the file/OS extensions. Status bytes
are mapped to text via the table transcribed from `navnet.dll` at VA
`0x18004ec00` (`device.rs::status_text`), which is where "unsupported file
type" and "file not found" come from.

## Layout

```text
src/protocol.rs   packet framing, checksums, ACK construction, golden-vector tests
src/transport.rs  USB (classic bulk) and CX II (NNSE) transports, discovery
src/device.rs     high-level file operations on the calculator
src/mcp.rs        MCP server exposing the file operations as tools
src/main.rs       egui GUI, calculator worker thread, CLI entry points
packaging/        desktop entry and icon used to build the AppImage
```

## Status and limitations

* Verified against a physical **TI-Nspire CX CAS** (PID 0xE012): connect,
  handshake, device info, directory listing, upload, download, mkdir, rename,
  copy and delete all confirmed working, both from the GUI and over MCP.
* **CX II / CX CAS 2 (PID 0xE022) is not tested.** The NNSE transport is
  implemented from the reversed format and the ACK path has unit tests, but it
  has never been run against CX II hardware. Treat CX CAS 2 support as
  unverified.
* The framing, command codes and payload layouts are pinned by the reversed
  binary and covered by unit tests (including a golden packet vector, the
  classic ACK state machine via a fake pipe, and date/status formatting).
* Uploads are validated by the calculator: only `.tns` names are accepted. The
  GUI renames other uploads with a prompt; the MCP tools do not rename.
* Deletes are confirmed in a modal (Escape cancels) and keyboard shortcuts are
  suppressed while a prompt is open, so a stray `Ctrl+A` + `Delete` cannot wipe
  a directory. Partial bulk-delete failures are reported as
  `Partial failure: …` rather than a plain success.
* A download whose announced size exceeds 256 MB is rejected before allocating,
  so a garbled header cannot OOM the process.
* Only the file-management and device-information services are implemented;
  OS install, screenshots and Navigator features are deliberately left out.

## Notes on the reverse engineering

Two things were confirmed the hard way while bringing this up against real
hardware:

1. The transport must not rewrite the destination service id. An early version
   kept its own copy and clobbered the file-manager sid (`0x4060`) with the
   handshake sid (`0x4003`), so the calculator silently ignored every request.
2. All USB calls run on a worker thread. libusb bulk transfers block for the
   full timeout when the device has nothing to send, which froze the UI when the
   calls ran on the event-loop thread.

## Legal

Interoperability reimplementation. The protocol description above is derived
from static analysis of a legally obtained copy of the TI software for the sole
purpose of talking to the user's own calculator. No TI code is included.

Licensed under the MIT license (see [`LICENSE`](LICENSE)).
