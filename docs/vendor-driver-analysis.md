# Vendor driver analysis

This document records behavioral facts derived through clean-room observation
and local static analysis. Vendor binaries are not part of this repository.

## Artifact

- Source: Elgato's official standalone Game Capture HD60 S driver download.
- Installer SHA-256:
  `30b006fb7e6c3beac9d3836da7901b26cc0b62debded748e4b2c38057af18e79`
- 64-bit driver: `CY3014.X64.SYS`
- Driver SHA-256:
  `a73163c84055186c00de46be90a2d3d660191b15b16f23463e7d4774f8d49451`
- INF history identifies driver version `1.1.0.192.4` dated 2021-02-03.

## Supported hardware revisions

The INF maps these Elgato USB product IDs to the same driver:

| Product ID | INF description |
|---|---|
| `004f` | HD60 S |
| `005e` | HD60 S Rev. 2 |
| `0074` | HD60 S Rev. 3 |
| `0076` | HD60 S Rev. 4 |

The development device is therefore revision 2.

## Confirmed implementation details

- The Windows implementation is an AVStream kernel minidriver with separate
  video and audio capture interfaces.
- It contains explicit branches for bulk and isochronous streaming. These match
  interface 0's observed alternate settings and endpoint `0x83`.
- It contains MCU boot-state, APROM/LDROM, security, firmware-version, EEPROM,
  EDID, and board-memory logic. Firmware-related paths must not be replayed while
  developing capture initialization.
- It exposes preview resolution, audio sample frequency, color conversion,
  cropping, frame-rate, and encoder properties.
- Static strings reference an internal `CDevice` implementation and an embedded
  driver build path. No symbols or source code are present.

## Current analysis anchors

Addresses below are virtual addresses for the exact driver hash above and are
not protocol constants:

- bulk/isochronous mode-selection function: `0x140228108`;
- MCU LDROM idle check: `0x140247ca4`;
- preview-video-resolution property path near `0x140242943`.

The next static-analysis task is to identify the lower-level USB request helper
called from the device initialization path, then express observed requests as a
new protocol description rather than copying driver code.

## Class-interface transfer layout

Static analysis identified the shared transfer constructor at `0x14025f9dc`.
For the exact driver hash above, it allocates an `0x88`-byte URB and sets:

- URB function `0x17` (`URB_FUNCTION_CLASS_INTERFACE`);
- transfer direction from an internal read/write argument;
- transfer buffer length and pointer;
- request byte at URB offset `0x81`;
- value at offset `0x82`;
- index at offset `0x84`.

This establishes that the device protocol uses USB class-interface control
requests, despite exposing vendor-specific interfaces.

## Read candidates rejected for live testing

The following read paths are structurally understood but are not suitable as
initial live probes because their callers participate in sensitive operations:

- request `0xc1`, value `0x0039`, index 0, length 1 is part of a bit-banged
  internal-register transaction surrounded by multiple writes;
- request `0xa0` is used for EEPROM reads;
- request `0xa6` is used for debug/board-memory reads;
- generic reads in the `0x14025a294` family participate in MCU presence and
  firmware verification.

No request from those paths should be executed merely to test the transport.
Normal capture traffic from the official driver is needed to identify a safe
initialization subset.
