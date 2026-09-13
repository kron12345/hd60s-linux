# Protocol notebook

This document distinguishes observations from hypotheses. Add packet sequences
only when a trace or repeatable experiment supports them.

## History: the Rev. 2 development unit (July 2026)

The project started on a Rev. 2 unit (`0fd9:005e`, firmware `25.4.15`).
Read-only probing established the USB topology — configuration 1 with
two vendor-specific interfaces, endpoint `0x83` with isochronous
alternate settings 1–3 and bulk alternate setting 4, interrupt endpoint
`0x81` on interface 1 — and that neither endpoint delivered anything
without a host sequence: claiming interface 1 saw no interrupt packets,
selecting bulk alternate setting 4 saw zero transfers, and the direct
class/interface read of bank `0x98` register `0x3b` (`hd60s-linux
startup-status`) got an I/O error. Static analysis of the Windows driver
(`vendor-driver-analysis.md`) recovered the register transports — the
direct form (request `0xc0`, `wValue` bank, `wIndex` register) and the
MCU-proxied form through `wValue 0x5066` — and a PnP sequence with an
MCU-presence handshake; replaying it blind was deliberately avoided.

Everything below was then measured on a Rev. 4 unit, where the proxied
form and the plug-in sequence turned out to be exactly what the static
analysis predicted. Whether Rev. 2 needs an extra enable step, or
something else was in the way, is still open (`roadmap.md`).

## Rev. 4 (`0fd9:0076`): confirmed video and audio transport

Everything in this section was measured on a Rev. 4 device (`0fd9:0076`,
firmware `25.4.15`, the same firmware version as the Rev. 2 development unit).
It is reproducible and not derived from the Windows driver.

### Streaming needs no initialization

On this device it is enough to claim interface 0 and select alternate setting
4 (bulk). Endpoint `0x83` then delivers data immediately, without any control
transfer, MCU handshake or register access. A five-second read returned
1,298,923,520 bytes, about 260 MB/s.

This differs from the Rev. 2 observation, where the same sequence produced zero
transfers. The microcontroller proxy, unlike the stream, does need the
driver's plug-in sequence first; see below.

### Framing

The stream is a sequence of BT.656-style timing reference codes:

```text
<ff 00 00 XY> <3840 B pixels, YUYV> [<12 B audio block>] <next ff 00 00 XY> ...
```

- `XY` has bit 7 set, followed by F, V and H, and carries the BT.656 protection
  bits computed from F/V/H in its low four bits. Exactly two values were
  observed: `0x80` (F=0 V=0 H=0, active line) and `0xab` (F=0 V=1 H=0, vertical
  blanking). Both satisfy the parity, which separates real markers reliably
  from a chance `ff 00 00` inside pixel data.
- Pixels are YUV 4:2:2, 8 bit, in `Y U Y V` order. A black picture reads
  `01 7f 01 7f ...`.
- At 1080p60 a frame has **1125 lines, 1080 of them active**, the SMPTE 274M
  raster. The frame period is **4,329,300 bytes**; times 60 that is 259.76 MB/s
  and matches the measured throughput.
- A frame starts where V changes from 1 to 0.

### Embedded audio

About 400 lines per frame carry, after the 3840 pixel bytes, an extra block of
12 bytes: a 4-byte header `ff 00 ff 02` followed by 8 bytes of payload. Such a
line is 3852 instead of 3840 bytes long.

400 blocks per frame times 8 bytes times 60 fps is 192,000 bytes/s, exactly
48 kHz stereo at 16 bit. Decoding as `s16le` stereo yields 2400 sample pairs
for three frames, which is 50.0 ms, the duration of three frames at 60 fps.

Confirmed with a sounding source on 2026-09-13: 1000 Hz on the left and 400 Hz
on the right channel were fed into the HDMI input (Radeon RX 7900 host,
PipeWire sink on the HDMI output the box is attached to). The blocks decoded as
`s16le` stereo gave 999.4 Hz left and 399.9 Hz right. Sample rate, word size,
signedness, byte order and channel order are therefore measured, not inferred
from the data rate alone.

With a silent source every sample sits at a constant `1`, not `0`.

Level check: a tone generated at amplitude 0.5 (-6 dBFS) with the sink at
100 % arrived with a peak of 16363 of 32767, that is 49.9 % of full scale.
The box passes audio at unity gain. (An earlier run measured only 264 because
FFmpeg's `sine` source emits at one eighth of full scale and the sink stood at
40 %; that was the test signal, not the device.)

### Software without PipeWire

`hd60s-linux capture` piped into `ffmpeg -f v4l2 /dev/video10` feeds a
v4l2loopback device. The module needs `exclusive_caps=1`, otherwise
browsers and OBS list the device but refuse to open it:

```text
options v4l2loopback devices=1 video_nr=10 card_label="Elgato HD60 S" exclusive_caps=1
```

### PipeWire

`hd60s-linux serve` publishes the capture through PipeWire: the video as a
camera node (via the `pipewire-vircam` crate, which drives the node with its
own clock), the audio as a virtual source that PipeWire itself creates
(`support.null-audio-sink` with media class `Audio/Source/Virtual`) and that
the tool feeds through a playback stream linked to it port by port. Two
things learned on the way: a stream published directly as `Audio/Source` has
no driver and is never scheduled, and the session manager links a playback
stream only to a sink — asking for another target quietly lands on the
default output. Recording from the virtual source works with its node name
as target; PipeWire 1.6 ignores a numeric id there.

### In practice

`hd60s-linux capture` writes the frames raw to stdout:

```bash
hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - ...
```

The device discards data during any gap between transfers. Decoding
between two synchronous `read_bulk` calls delivered only 29.1 complete fps
(308 incomplete frames in 10 s); a separate reader thread with 1 MB buffers
57.8 to 59.6 fps; and even then every register read from another thread
widened the gap between one read's completion and the next submission
(56.1 fps, 230 damaged frames in 60 s with the panel and the tray active).
Eight asynchronous 1 MB transfers kept queued in the kernel and resubmitted
from their completion callbacks close the gap for good: 59.96 fps and no
damaged frames under the same load (`pump::bulk_reader`).

### What the official driver does (usbmon trace of a Windows VM, Rev. 4)

Recorded on 2026-09-13 with the Elgato driver `CY3014.X64.SYS` and the Game
Capture HD application in a QEMU guest, `usbmon` on the host. Raw traces stay
private (they contain the serial and captured video).

- **All register traffic is vendor/device** (`bmRequestType 0x40` OUT, `0xc0`
  IN), not class/interface as the static analysis assumed. The direct form is
  `bRequest 0xc0`, `wValue` = bank, `wIndex` = register. The decoder and the
  analyzer now accept that form; the earlier live probe used class/interface,
  which is a plausible reason for its I/O error.
- **Rev. 4 works on bank `0x64`** where the Rev. 2 analysis found `0x98`.
- **PnP sequence** 2.3 s after enumeration, in this order: `0xec` OUT (10 B),
  `0xc1` OUT `wValue 0xc039` and `0x4134`, `0xc1` IN `0x0039` (1 B → `01`),
  proxied read `0xc0 OUT wValue 0x5066` with payload `ab 03 12 34 57` followed
  by ten 3-byte `0x5066` IN reads, the same with `... 34 58`, then `0xc2`,
  `0xc7 wValue 0x64`, `0xc6 wIndex 0x100`, and bank `0x64` writes: register
  `0x13` := `80 80 80 80`, `0x3a` := `00`, `0x3b` := `80`. Afterwards the
  driver reads bank `0x64` register `0` (32 bytes) every 105 ms for as long as
  it is loaded; the value never changed with a stable 1080p60 input.
- **Stream start** by the application: bank `0x64` register `0x3b` := `80`,
  register **`0x10` := `01`**, then `SET_INTERFACE` alternate setting **2**
  (isochronous). Video follows 38 ms later at about 250 URBs/s. **Stream stop**
  is the mirror image: alternate setting 0, then register `0x10` := `00`. The
  official path is therefore isochronous; the bulk path (alternate setting 4)
  used by `capture` needs none of this and keeps working. Replayed from Linux
  with `hd60s-linux observe-iso SECONDS` (libusb asynchronous isochronous
  transfers, 32 packets of 32 KB per transfer, eight in flight): 259.6 MB/s,
  8000 packets per second, and the data is framed exactly like the bulk
  stream — same markers, same lines, same audio trailers.
- **Bank `0x64` registers written by the application** (traced by changing
  each setting of the Game Capture HD application one at a time):
  - `0x10`: stream on (`01`) / off (`00`). Every profile or frame-rate change
    in the application is just a stop and a start of the stream.
  - `0x12`: colour range conversion — per the analysis in dougg3's kernel
    driver `0` = bypass, `1` = shrink, `2` = expand (the receiver clamps luma
    at 235 regardless, so expand cannot reach full range). The application's
    "Expanded" setting and its "PC" preset write `1`; "Standard" and the
    PlayStation preset write `0`; the Xbox presets leave it alone.
  - `0x13`: four picture controls, one byte each — brightness, contrast,
    saturation, hue — with `80` as neutral. "Brightness up" wrote
    `86 80 80 80`, "Reset Defaults" `80 80 80 80`. The `80 81 80 80` written
    1.5 s after every stream start and reverted 3 s later is a brief +1
    contrast nudge, presumably to make the receiver re-apply the controls.
  - `0x3b`: audio gain. `00` mutes, `80` is 0 dB and each step is about
    0.5 dB (measured with a test tone through the capture: -32 dB at `40`,
    -16 dB at `60`, clipping above `a0`). This is the register behind the
    application's "Analog Audio Gain" slider (-12..+12 dB ≈ `68`..`98`),
    which the driver also sets to `80` at PnP and before every stream start.
    It affects the captured audio only; the bulk and isochronous video paths
    do not react to it.
  - `0x3a` and `0x3c` (set to `00` and `80` at PnP): no observed effect on
    video, timing, audio, or the isochronous path for any value tried.
    `0x0d` (`12`) and `0x0e` (`30`) sit in the timing block and read back
    unchanged after a write — status, not configuration.
  - The application never rewrites the EDID for any setting; it does not
    force an input resolution on the source.
  - `hd60s-linux edid` reads the EDID from bank `0xa0`, saves it (`--dump`),
    writes one from a file (`--write`, refused unless header and both block
    checksums are valid, `--fix` recomputes them) or restores the power-on
    block (`--restore`), then reads it back. This is the only way to tell a
    source what it may send — the application never does it, but with it a
    console or camera can be limited to 1080p or 720p. The source re-reads
    the EDID on hot-plug only.

### Microcontroller commands

The proxy `0x5066` carries commands to the device's microcontroller: an OUT
request with payload `ab 03 12 34 <command>`, then IN requests that return a
3-byte reply once the MCU has processed it (the buffer keeps its previous
content until then, so the driver polls). The driver issues `0x57` and
`0x58` at PnP time. Measured replies on the Rev. 4 unit: `51 10 27` for
`0x57`, `14 09 18` for `0x58`, `51 10 18` for `0x59`. Their meaning is not
established (the MCU firmware image that ships with the Windows application
answers these commands with different bytes, so it is not the firmware this
unit runs). `hd60s-linux mcu` issues exactly these three.

**Command `0x60` must never be sent.** It turns the light strip on, unlocks
the system registers, sets the boot-select bit and resets the MCU into its
bootloader — the firmware-update entry point. The tool refuses anything
outside the three status commands. (Established by reading the MCU firmware
image that ships with the Windows application; the image itself is not part
of this repository, and the unit at hand runs a different firmware build, so
the command is avoided on the assumption that the entry point is shared.)

There is no USB command for the light strip; see the note on it above.

### The plug-in sequence is required from power-on (measured 2026-09-13)

Every Linux measurement before this date was made on a card that the
official driver had initialised earlier (during the Windows VM traces) and
that had only been USB-reset since, never power-cycled. From a **true
power-on state** the card behaves differently:

- Standard requests and bank `0x64` reads work immediately.
- The **first MCU proxy command (`0x5066`) without the plug-in sequence
  hangs the card's USB controller**: the read times out, and from then on
  every request on endpoint 0 — vendor requests, `GET_STATUS`, even
  `GET_DESCRIPTOR` through usbfs — times out or fails with an I/O error,
  while the HDMI side (EDID to the source, pass-through) keeps working.
  A libusb reset, the sysfs `authorized` toggle, a hub-port disable, an
  xHCI unbind/rebind, a PCI function reset and a PCI remove/rescan of the
  controller do not help (this root hub cannot switch VBUS); only unplugging
  the USB cable recovers it, and it then hangs again at the next bare proxy
  command.
- With the official driver's plug-in sequence sent first, the proxy works
  as always (trace T04: after a plain replug, the Windows driver ran
  `0xec` → `0xc1` → proxy → `0xc2`/`0xc7`/`0xc6` → register writes and got
  `51 10 27` / `14 09 18` as usual). Replaying the same sequence from Linux
  gives the same result.

The sequence, sent verbatim by `Control::initialise` before anything else:

| Request | wValue | wIndex | Data |
|---|---|---|---|
| OUT `0xec` | 0 | 0 | `b8 22 00 00 00 c0 00 00 50 ca` |
| OUT `0xc1` | `0xc039` | 0 | – (stalls when already initialised; ignored) |
| OUT `0xc1` | `0x4134` | 0 | – |
| IN `0xc1` | `0x0039` | 0 | answers `01` |

`0xc2`, `0xc7`/`0x0064` and `0xc6`/`0x0100` (dougg3: disarm event
reporting) follow in the driver's sequence but were not needed for the
proxy, the registers or streaming and are not sent.

The reply buffer of the proxy keeps its previous content (`33 44 55` after
power-on) until the microcontroller has answered, about 100 ms later, and
the three bytes are updated one at a time (`14 10 27` was seen between
`14 09 18` and `51 10 27`); a reader must wait and poll until two reads
agree.

### Register access while streaming

The official driver keeps using the control endpoint while it streams —
the 105 ms poll of bank `0x64` register `0x00` and the picture writes —
through the same device handle that owns the bulk pipe. Doing the same from
Linux through the streaming handle works (`serve` and its panel: timing and
settings reads, picture/range/gain writes during a 1080p60 stream, no frame
loss). The first attempt from a *second* handle in another process, which
issued a bare proxy command, produced the hang described above; whether a
second handle by itself is harmful was not separated from the missing
plug-in sequence. The tools keep one handle per process, refuse register
access from the command line while another process holds the streaming
interface, read the microcontroller and the EDID only before streaming, and
run the plug-in sequence on every open.

### Bank `0x64` registers `0x00`–`0x1f`: the input timing

The 32-byte read the driver polls is a window on registers `0x00`–`0x1f`
of bank `0x64` (a write to `0x10` shows up at byte 16). Bytes 4–11 hold the
detected input timing as little-endian 16-bit words; with a 1080p60 source:

```text
00 00 00 00 | 65 04 | 98 08 | 38 04 | 80 07 | 3c 12 30 00 | 00 00 80 80 80 80 80 00 ...
              1125    2200    1080    1920    60 ...
              total   total   active  active  Hz
              lines   pixels  lines   pixels
```

Unplugging the HDMI cable zeroes bytes 4–12 within the 100 ms poll interval;
plugging it back restores them about nine seconds later. `hd60s-linux signal
[SECONDS]` reads and watches this. Unplugging and replugging the HDMI
**output** (the pass-through to a monitor) changes nothing: no register
moves, the strip stays dark, the stream continues. The box does not watch
its output, which matches it presenting its own EDID to the source. Bytes 13 (`0x12`) and 14 (`0x30`) did not
change with the signal; their meaning is open. The bulk stream survived the
unplug; the feeder service did not need a restart.

### Other input formats

Measured on 2026-09-13 by switching the source (a Radeon RX 7900) through
its modes. Every one uses the same framing; line length is width × 2, the
number of active lines is the height, and the timing registers announce both:

| Source | Timing registers | Active + blanking lines | Bytes per line | Frame period |
|---|---|---|---|---|
| 1080p60 | 1920×1080, 2200×1125, 60 | 1080 + 45 | 3840 | 4,329,300 |
| 1080p50 | 1920×1080, 2640×1125, 50 | 1080 + 45 | 3840 | 4,330,260 |
| 1080p30 | 1920×1080, 2200×1125, 30 | 1080 + 45 | 3840 | 4,333,500 |
| 720p60 | 1280×720, 1650×750, 60 | 720 + 30 | 2560 | 1,927,784 |
| 1280×1024@60 | 1280×1024, 1712×1063, 60 | 1024 + 39 | 2560 | 2,730,344 |
| 576p50 | 720×576, 864×625, 50 | 576 + 49 | 1440 | 907,680 |
| 480p60 | 720×480, 858×525, 60 | 480 + 45 | 1440 | 762,408 |
| output off | no signal | — | no transfers at all | — |

All of these are progressive (F = 0 throughout); the source cannot produce an
interlaced signal, so that case remains unmeasured. With no signal the device
sends nothing — no black frames, zero bulk transfers.

The assembler now takes width and height from the stream itself and reports a
format change when they differ from the previous frame. `capture` places
every frame centred on a 1920×1080 canvas by default, so the output keeps one
size across source changes; `capture --native` writes frames at source size.

### Still open

- Interlaced sources (the F bit is parsed but not yet used); every source
  available here is progressive.
- Behaviour with an HDCP-protected source.
- Purpose of the isochronous alternate settings 1 and 3 (the official
  application uses 2).
- Interrupt endpoint `0x81`, silent even under the official driver and
  application (which disarms event reporting with `0xc6`).
- Whether Rev. 1 to 3 stream without initialization as well.
- Whether register access from a second handle next to a stream is harmful
  by itself (the hang of 2026-09-13 is explained by the missing plug-in
  sequence).
- Meaning of the `0xec` payload and of requests `0xc2` and `0xc7`.
