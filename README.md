# hd60s-linux

Experimental native Linux support for the **original** Elgato Game Capture
HD60 S. This model uses USB ID `0fd9:005e` and is not a USB Video Class device.
The HD60 S+ is different hardware and is outside this project's scope.

No working capture driver exists here yet. The first objective is to document
the proprietary protocol without guessing, then implement it in userspace with
libusb. Do not use this project for firmware updates.

## Current milestone: USB characterization

The `hd60s-linux` binary locates the device and prints its configurations,
interfaces, alternate settings, and endpoints:

```bash
cargo run
```

Observe the device's interrupt endpoint without sending vendor commands:

```bash
cargo run -- observe 10
```

Perform a bounded read of the advertised bulk stream alternate setting:

```bash
cargo run -- observe-stream 10
```

Read the first confirmed volatile startup status register without writing:

```bash
cargo run -- status
```

On a power-on Rev. 2 device this currently returns a USB I/O error, confirming
that an earlier volatile enable sequence is required. The command remains a
bounded protocol diagnostic, not a working signal-status query.

Run tests with:

```bash
cargo test
```

Access may require a udev rule. During development, grant only the device's
interactive desktop user access rather than making it world writable:

```udev
SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="005e", TAG+="uaccess"
```

Install the included rule and reconnect the device:

```bash
sudo install -Dm644 udev/70-hd60s-linux.rules \
  /etc/udev/rules.d/70-hd60s-linux.rules
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=usb
```

## Intended architecture

```text
HD60 S -- proprietary USB --> hd60sd (libusb)
                                  |-- protocol state machine
                                  |-- video/audio packet reconstruction
                                  |-- timestamps and recovery
                                  +--> Unix socket / stdout
                                            |
                                            +--> FFmpeg / OBS
                                            +--> PipeWire or V4L2 loopback later
```

Protocol work stays in userspace until initialization and streaming are stable.
A kernel module would make early reverse engineering harder and failures riskier.

See [docs/protocol.md](docs/protocol.md) and [docs/roadmap.md](docs/roadmap.md).
The provisional OBS transport and test procedure are in
[docs/obs.md](docs/obs.md).
Reproducible facts from the official Windows driver are recorded in
[docs/vendor-driver-analysis.md](docs/vendor-driver-analysis.md).
The exact continuation state and next-session checklist are in
[docs/next-session.md](docs/next-session.md).
The QEMU host-side USB capture and sanitization workflow is documented in
[docs/tracing.md](docs/tracing.md).

## Legal and safety boundaries

Contributors must capture traffic from hardware and software they are authorized
to use. Do not commit Elgato firmware, Windows drivers, HDCP keys, device serial
numbers, or copyrighted binary blobs. Capture fixtures must be sanitized before
publication.

## License

MIT
