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

