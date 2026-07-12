# Next-session handoff

## Objective

Recover the original HD60 S normal capture initialization protocol, implement
it incrementally in the Rust userspace driver, reconstruct video/audio from USB
endpoint `0x83`, and feed the existing localhost MPEG-TS transport into OBS.

## Current state

- Repository: `https://github.com/Hydrogen2K/hd60s-linux`
- Local checkout: `/home/hayden/hd60s-linux`
- Latest completed commit before this handoff update: `83beebf`.
- Branch: `main`, clean and tracking `origin/main`
- Device: Elgato HD60 S Rev. 2, USB `0fd9:005e`, device version `25.4.15`
- Installed probe: `/home/hayden/.local/bin/hd60s-linux`
- Udev access works; the probe opens the device without elevated privileges.
- OBS 32.1.2 and FFmpeg are installed.
- OBS transport test is implemented and verified at 1080p60 H.264 plus 48 kHz
  AAC over `udp://127.0.0.1:5000`.
- Read-only interrupt observation on `0x81` returned no unsolicited packets.
- Selecting interface 0 bulk alternate setting 4 and reading `0x83` returned no
  transfers. A proprietary initialization sequence is required.
- The official Elgato standalone driver was downloaded and extracted locally
  under ignored `research/` directories. Do not commit these binaries.
- A local ignored Rizin toolchain is available under `research/tools/`.
- QEMU, `tshark`, KVM access, and a Windows ISO were still absent at the final
  prerequisite audit.
- Host usbmon capture and sanitized TSV decoding scripts now exist as
  `tools/capture-usbmon` and `tools/decode-usb-trace`.
- The installed `status` command reconstructs the first normal HDMI register
  read, but the power-on device rejects it with an I/O error. The card continues
  to enumerate normally. This proves an earlier PnP-time volatile enable remains
  missing; do not add writes to work around the stall.

## Verified protocol facts

- Interface 0 endpoint `0x83` supports isochronous alternate settings 1-3 and
  bulk alternate setting 4; maximum packet size is 1024.
- Interface 1 exposes interrupt IN endpoint `0x81`, maximum packet size 64.
- The Windows implementation uses `URB_FUNCTION_CLASS_INTERFACE` (`0x17`).
- Its shared constructor stores request, value, and index at URB offsets `0x81`,
  `0x82`, and `0x84`, respectively.
- The driver supports product IDs `004f`, `005e`, `0074`, and `0076`.
- Driver and installer hashes and static-analysis anchors are recorded in
  `docs/vendor-driver-analysis.md`.
- Normal register access uses class-interface request `0xc0`, with `wValue` as
  the bank/device, `wIndex` as the register, and a one-byte payload.
- Input mode 4 selects the compact HDMI initializer. Its statically recovered
  bank `0x98` sequence begins with register `0x3b`, followed by writes to `0x20`,
  `0x00`, and `0x10`; those writes remain intentionally disabled pending a trace.

## Safety boundary

Do not execute USB operations recovered from these paths:

- firmware update or verification;
- EEPROM or board-memory access;
- EDID writes;
- APROM/LDROM or bootloader state changes;
- MCU security or unlock operations;
- requests `0xa0` or `0xa6` discovered in sensitive read paths;
- `c1/0039` internal-register access without its complete surrounding sequence.

Do not issue a live class-interface write until it appears in a normal capture
trace, its position and payload are understood, and it has been classified as
volatile. Stop and document ambiguity instead of testing unknown writes.

## Prerequisites for next session

The user must run:

```bash
sudo pacman -S --needed qemu-desktop edk2-ovmf swtpm wireshark-cli
```

Obtain an official Windows 10 or 11 ISO and record its absolute path. QEMU and a
Windows ISO were not present at the end of this session.

## Next steps

1. Implement a Rust analyzer for the sanitized TSV trace format. It must classify
   known volatile bank `0x98`/`0x9c` register traffic, automatically redact
   EEPROM/board-memory/MCU payloads, label unknown requests, summarize endpoint
   `0x81`/`0x83` activity, and include synthetic golden tests that do not require
   `tshark`.
2. Verify QEMU, OVMF, KVM access, `tshark`, the ISO path, free disk space, and
   USB access.
3. Update `/home/hayden/hd60s-linux-bridge/hd60s-vm` if necessary and create a
   64 GiB Windows VM disk with USB passthrough for `0fd9:005e`.
4. Install only the official standalone HD60 S driver in the guest first.
5. Establish host-side packet capture using Linux `usbmon` and `dumpcap`.
6. Record and hash a baseline enumeration trace with HDMI disconnected.
7. Record preview start/stop traces for 720p60, 1080p30, and 1080p60 SDR.
8. Sanitize serial numbers and identifying fields before adding trace-derived
   fixtures. Never commit the raw driver or unsanitized capture.
9. Diff enumeration, initialization, steady-state stream, and shutdown phases.
10. Decode class-interface transfers into direction, request, value, index,
   length, payload, response, repetition count, and timing.
11. Classify each write as volatile capture configuration or prohibited
    persistent/maintenance behavior.
12. Implement the smallest confirmed read-only status subset in Rust with
    golden-fixture tests.
13. Implement capture initialization one volatile stage at a time with bounded
    timeouts, stage logs, and an external process timeout.
14. Capture raw endpoint `0x83` data and identify framing or standard media
    signatures before adding a parser.
15. Reconstruct H.264/AAC or raw video/audio, then send it through the already
    validated MPEG-TS transport documented in `docs/obs.md`.

## Resume commands

```bash
cd /home/hayden/hd60s-linux
git status --short --branch
git pull --ff-only
cargo test --locked
~/.local/bin/hd60s-linux
```

Inspect the existing research notes before touching hardware:

```bash
sed -n '1,240p' docs/protocol.md
sed -n '1,260p' docs/vendor-driver-analysis.md
sed -n '1,220p' docs/obs.md
```
