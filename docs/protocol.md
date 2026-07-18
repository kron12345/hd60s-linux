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
