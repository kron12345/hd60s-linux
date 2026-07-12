# Roadmap

## M0: Characterize USB topology

- Enumerate known product revisions.
- Record configurations, interfaces, alternate settings, and endpoints.
- Add sanitized descriptor fixtures and parser tests.

## M1: Capture and decode Windows traffic

- Produce the trace experiment matrix in `protocol.md`.
- Build a decoder for control and streaming transfers.
- Document the capture state machine from observed traffic.

## M2: Read-only userspace prototype

- Initialize the device without persistent writes.
- Detect HDMI signal state.
- Receive streaming endpoint data into bounded buffers.
- Save the unmodified stream for offline analysis.

## M3: Reconstruct media

- Recover video frames and audio packets.
- Establish timestamps and A/V synchronization.
- Feed a standard container or elementary stream to FFmpeg.

## M4: Operational daemon

- Handle signal changes, disconnects, USB resets, and backpressure.
- Expose metrics and structured protocol logs.
- Add a stable Unix socket interface and OBS integration.

## Deferred

- PipeWire and V4L2 loopback integration.
- Kernel driver work.
- Other Elgato product IDs.

