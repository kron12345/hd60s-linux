# Protocol notebook

This document distinguishes observations from hypotheses. Add packet sequences
only when a trace or repeatable experiment supports them.

## Confirmed

- Target product: original Elgato Game Capture HD60 S.
- USB vendor/product ID on the initial test unit: `0fd9:005e`.
- Initial test unit reports USB 3.0 and device version `25.4.15`.
- Configuration 1 has two vendor-specific (`ff/00/00`) interfaces.
- Interface 0 endpoint `0x83` is IN and offers:
  - alternate setting 0 with isochronous maximum packet size 0;
  - alternate settings 1 through 3 with isochronous maximum packet size 1024;
  - alternate setting 4 as bulk with maximum packet size 1024.
- Interface 1 has interrupt IN endpoint `0x81`, maximum packet size 64 and
  interval 7.
- The device does not expose a Linux V4L2 node through `uvcvideo`.
- HDMI passthrough operates independently of a Linux capture driver.

## Unknown

- Runtime meaning and ordering of the PnP-time class control requests.
- Whether runtime firmware is uploaded by the host.
- Video transport: encoded, raw, or proprietary framing.
- Audio transport and clock source.
- Signal detection and resolution negotiation messages.
- Start, stop, reset, and error-recovery sequences.

## Experiment log

- A read-only attempt to open the device and claim interface 1 appeared to block
  for more than 30 seconds and was terminated before stage logging was added.
  The device enumerated normally afterward.
- A subsequent instrumented run confirmed no kernel driver was bound, claimed
  interface 1 successfully, and received zero unsolicited packets from interrupt
  endpoint `0x81` over three seconds.
- A bounded read claimed interface 0, selected advertised bulk alternate setting
  4, and received zero transfers from endpoint `0x83` over three seconds. The
  alternate setting change completed normally and the local capture was empty.
- Together, the endpoint observations indicate that a host control sequence must
  arm notifications and streaming before either endpoint becomes active.
- A reconstructed direct-form class-interface read for bank `0x0098`, register
  `0x003b` was attempted with a one-second libusb timeout. The power-on device
  rejected it with an I/O error and continued to enumerate normally afterward.
  Later static analysis showed that the official Rev. 2 PnP path can select an
  MCU-proxied register transport instead. The failure therefore rejects the
  direct framing in this state; it does not by itself prove that a simple enable
  write is missing.

## Statically recovered register transport variants

The Windows driver has direct and MCU-proxied variants behind the same generic
register helpers. These are static facts awaiting runtime trace confirmation on
the development device.

- Direct access uses request `0xc0`, `wValue` as bank, `wIndex` as register, and
  the register payload directly.
- A proxied write to bank `0x98` or `0x9c` uses class-interface OUT request
  `0xc0`, `wValue` `0x5098` or `0x509c`, `wIndex` zero, and a payload beginning
  with the register followed by its data.
- A proxied read first uses class-interface OUT request `0xc0`, `wValue`
  `0x5066`, `wIndex` zero. A one-byte bank `0x98`, register `0x3b` read is encoded
  as payload `99 01 3b`. A class-interface IN request with value `0x5066` then
  obtains the response.

The official Rev. 2 PnP branch reaches MCU-presence and internal-register logic
before normal HDMI register access. That surrounding sequence remains
prohibited from live replay until it appears in a normal official-driver trace.

## Trace experiment matrix

Record each experiment from USB connection through clean capture shutdown:

| ID | HDMI input | Capture mode | Action |
|---|---|---|---|
| T00 | disconnected | none | enumerate only |
| T01 | 720p60 SDR | preview | start, 10 seconds, stop |
| T02 | 1080p30 SDR | preview | start, 10 seconds, stop |
| T03 | 1080p60 SDR | preview | start, 10 seconds, stop |
| T04 | 1080p60 SDR | preview | disconnect/reconnect HDMI |

For every trace, record device revision, firmware version, input generator,
driver version, application version, and SHA-256 checksum. Do not publish a
trace until serial strings and potentially identifying payloads are removed.

## Analysis method

1. Separate enumeration, initialization, steady-state streaming, and shutdown.
2. Diff control transfers across T00 through T03.
3. Identify endpoint direction, transfer type, packet size, and cadence.
4. Search payloads for MPEG-TS sync bytes, H.264 start codes, JPEG markers,
   audio frame headers, counters, and timestamps.
5. Replay the smallest confirmed initialization prefix with libusb.
6. Treat every unexplained write as unsafe until its effect is understood.

## Rev. 4 (`0fd9:0076`): confirmed video and audio transport

Everything in this section was measured on a Rev. 4 device (`0fd9:0076`,
firmware `25.4.15`, the same firmware version as the Rev. 2 development unit).
It is reproducible and not derived from the Windows driver.

### No initialization command is needed

On this device it is enough to claim interface 0 and select alternate setting
4 (bulk). Endpoint `0x83` then delivers data immediately, without any control
transfer, MCU handshake or register access. A five-second read returned
1,298,923,520 bytes, about 260 MB/s.

This differs from the Rev. 2 observation, where the same sequence produced zero
transfers. Whether Rev. 2 really needs an enable step, or something else was in
the way there, is open.

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

### OBS and other V4L2 clients

`tools/hd60s-obs` feeds the frames into a v4l2loopback device and the audio
into a PipeWire null sink, so OBS sees a camera named "Elgato HD60 S" and a
source "Monitor of Elgato HD60 S". `tools/hd60s-obs.service` runs it as a
systemd user unit; the header of that file shows a udev rule that starts it
on hotplug. The loopback module needs `exclusive_caps=1`, otherwise browsers
and OBS list the device but refuse to open it:

```text
options v4l2loopback devices=1 video_nr=10 card_label="Elgato HD60 S" exclusive_caps=1
```

### In practice

`hd60s-linux capture` writes the frames raw to stdout:

```bash
hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - ...
```

Reading must run in its own thread. Decoding between two `read_bulk` calls
makes the hardware discard data during that pause: in that form only 29.1
complete fps arrived, with 308 incomplete frames in 10 seconds. With a
decoupled reader thread and 1 MB buffers it is 57.8 to 59.6 fps with 0 to 19
incomplete frames.

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
  used by `capture` needs none of this and keeps working.
- 1.5 s after the stream starts the application writes register `0x13` :=
  `80 81 80 80` and 3 s later `80 80 80 80` again. Purpose unknown.
- Before starting, the application reads the **EDID from bank `0xa0`**
  (256 bytes as 16 reads of 16 bytes at `wIndex` 0, 16, … 240), writes it
  back, then writes a version with the monitor name changed from "Elgato" to
  "HD60 S". The EDID the box presents to the HDMI source is therefore writable.
- **Interrupt endpoint `0x81` carried no data at any point**, not even with the
  official application streaming.
- **The light strip is driven by the firmware, not by the host.** It is an
  RGB strip: at power-on it blinks red twice and then white, it lights red
  while no HDMI signal is present, and it stayed off with the driver loaded
  and while the application streamed. Replaying the driver's writes from Linux
  (`0x13` := `80 81 80 80`, and `0x3b` := `80` followed by `0x10` := `01`)
  changed nothing visible.

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

### Still open

- Behaviour with other input resolutions and frame rates, including interlaced
  sources (the F bit is parsed but not yet used).
- Behaviour on signal loss, with an HDCP-protected source, and on a resolution
  change while streaming.
- Purpose of the isochronous alternate settings 1 and 3 (the official
  application uses 2).
- Meaning of interrupt endpoint `0x81`, which stayed silent even under the
  official driver and application.
- Whether Rev. 1 to 3 stream without initialization as well.
