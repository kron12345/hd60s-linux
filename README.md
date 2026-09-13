# hd60s-linux

Experimental native Linux support for the **original** Elgato Game Capture
HD60 S, which is not a USB Video Class device. The HD60 S+ is different
hardware and is outside this project's scope.

The official Windows driver serves four hardware revisions under one INF:
`0fd9:004f`, `0fd9:005e` (Rev. 2), `0fd9:0074` (Rev. 3) and `0fd9:0076`
(Rev. 4). All four are recognized here.

**On Rev. 4 (`0fd9:0076`), `hd60s-linux capture` delivers 1080p60 video and
48 kHz stereo audio without sending any vendor command.** The stream format is
documented in `docs/protocol.md`. The Rev. 2 development unit returned no
stream data under the same sequence, so the other revisions are not yet
confirmed to work. The protocol is documented from measurements, not guessed.
Do not use this project for firmware updates.

## Capturing (Rev. 4)

Frames are written raw to stdout as YUYV 4:2:2, 1920x1080, 60 Hz:

```bash
cargo run --release -- capture | \
  ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - output.mkv
```

`capture SECONDS` stops after a bounded time; `--audio FILE` additionally
writes the embedded audio as raw `s16le` stereo 48 kHz. The USB read runs in
its own thread because the hardware drops data during any pause between
transfers, and the outputs are written from their own threads: a consumer
that stalls costs frames (counted in the periodic statistics) but never
blocks the capture.

For OBS, browsers and other V4L2 clients, `tools/hd60s-obs` feeds video into a
v4l2loopback device and audio into a PipeWire sink; `tools/hd60s-obs.service`
keeps it running as a systemd user unit. See `docs/protocol.md` for the
module options.

Read the detected HDMI input timing, or watch it for a while:

```bash
cargo run --release -- signal      # once
cargo run --release -- signal 60   # watch, print changes
```

## USB characterization

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

On a power-on Rev. 2 device this currently returns a USB I/O error. Static
analysis now shows that this is only the direct fallback form of register
access. The official driver first performs a sensitive MCU-presence handshake
and can then use a tunneled register form. The command remains a bounded direct-
transport diagnostic, not a working signal-status query; the handshake and
proxy sequence are intentionally not implemented without a trace.

Run tests with:

```bash
cargo test
```

Analyze a decoded USB trace and write a conservatively redacted TSV with
protocol classifications:

```bash
cargo run --locked --bin analyze-usb-trace -- \
  research/traces/T01-1080p60-start-stop.tsv \
  research/traces/T01-1080p60-start-stop.sanitized.tsv
```

Only confirmed volatile register traffic retains payloads. Sensitive and
unknown control requests, interrupt payloads, and captured media are redacted.
The analyzer also prints request classifications and endpoint activity totals.
Its synthetic golden tests do not require Wireshark.

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
