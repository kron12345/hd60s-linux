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

## Install

Arch Linux (and derivatives), from the `packaging/` directory:

```bash
cd packaging && makepkg -si
systemctl --user enable --now hd60s-serve.service
```

Debian/Ubuntu, with [cargo-deb](https://github.com/kornelski/cargo-deb):

```bash
cargo install cargo-deb && cargo deb
sudo apt install ./target/debian/hd60s-linux_*.deb
systemctl --user enable --now hd60s-serve.service
```

Both install the binary, a udev rule that lets the logged-in user open the
device (that is the only step that needs root, and the package does it) and
starts the service when a card is plugged in, and the user unit. Nothing
runs as root and no kernel module is involved.

## Use it as a camera (PipeWire, no kernel module)

```bash
cargo run --release -- serve
```

publishes the device as a PipeWire camera **"Elgato HD60 S"** and a virtual
microphone **"Elgato HD60 S Audio"**. OBS (Sources → Video Capture Device
(PipeWire) and Audio Input Capture), browsers through the camera portal, and
`gst-launch-1.0 pipewiresrc target-object="Elgato HD60 S"` see them like any
camera. Nothing needs root: the udev rule below grants device access, and a
kernel driver bound to the device is detached automatically. Frames are
always 1920x1080 YUYV (smaller sources are centred on a black canvas), audio
is 48 kHz stereo.

The USB read keeps eight 1 MB bulk transfers queued in the kernel at all
times (libusb's asynchronous API), because the device drops data during
any gap between transfers: measured over 60 s with the panel polling the
preview and the tray refreshing, this delivers 59.96 fps with no damaged
frames, where one synchronous read at a time lost about 4 frames a second
to the gaps that register reads from other threads opened.

The service waits for a card if none is present and picks it up again after
an unplug or reset; the camera and microphone nodes stay in place meanwhile
(viewers see black and silence), so OBS keeps its sources. Run only one
instance: a second one finds the interface busy and just keeps waiting.

`tools/hd60s-serve.service` runs it as a systemd user unit; `--name` changes
the node names; `--from-file RAW [--fps N]` replays a recorded raw stream
instead of the device, for development without hardware.

### Control program

`hd60s-control` (in the application menu as *HD60 S Control*, or from the
tray icon) is the native window for the service: the live picture,
picture controls, colour range and audio gain, device, EDID and
microcontroller status, recording, the network stream switch and the
report. It is written in Rust with [Slint](https://slint.dev) and runs on
Wayland (Plasma, sway) and X11. It talks to the service over
`$XDG_RUNTIME_DIR/hd60s-linux/api.sock`, which only the same user can
reach — no port, no token, nothing a browser can get at. The web panel
below offers the same over HTTP for remote or scripted use.

### Control panel

While `serve` runs it also serves a control panel on
<http://127.0.0.1:8060/> (`--panel ADDR` moves it, `--panel off` disables
it; it is bound to localhost and has no authentication). The page shows
everything the card exposes and lets you change what can be changed:

- a live preview of the captured picture, the detected input timing and the
  stream statistics (frames, frame rate, bad frames, format changes);
- brightness, contrast, saturation, hue, the colour range and the audio
  gain — applied to the card as you move the sliders;
- the device identity (revision, USB speed, firmware version, MCU build
  date), the microcontroller status replies, the EDID (download it or go
  back to the power-on block; writing pauses the stream for a moment) and a
  report for bug reports.

The same is available as JSON under `/api/state`, `/api/set?brightness=…`
and friends, `/preview.jpg`, `/edid.bin` and `/api/report`, for scripts.

Reading is open to anything on this machine; **every change needs a
token** that is new at each start, embedded in the page and written to
`$XDG_RUNTIME_DIR/hd60s-linux/token` (mode 0600) for scripts:

```bash
curl -X POST -H "X-Token: $(cat "$XDG_RUNTIME_DIR/hd60s-linux/token")" \
  "http://127.0.0.1:8060/api/record?start=1"
```

A web page you happen to visit cannot use the panel: it does not know the
token, the custom header forces a CORS preflight the panel refuses, a
foreign `Origin` is rejected, and a `Host` other than localhost (DNS
rebinding) is rejected too. What remains is that other user accounts on
the same machine can read the state; a Unix-socket API is the planned
answer for that.

**Only one process may talk to the card at a time.** The command-line tools
refuse to touch the registers while `serve` holds the streaming interface —
use the panel, or stop the service first. Every open runs the official
driver's plug-in sequence first; without it a microcontroller query hangs
the card's USB controller until it is unplugged (see `docs/protocol.md`).
If that ever happens: unplug the USB cable for a few seconds and plug it
back — a USB reset is not enough — then use a current build.

### Recording

The panel's record button and the tray menu write the stream to a file:
`ffmpeg` encodes the 1920x1080 frames and the audio to H.264 + AAC in a
Matroska file named `HD60 S <date> <time>.mkv` in your Videos directory
(`--record-dir DIR` changes that). The encoder is chosen automatically: the
GPU's VA-API H.264 encoder when a test encode on `/dev/dri/renderD128`
succeeds (Intel, AMD), libx264 otherwise; `--record-encoder x264|vaapi`
forces one, and the panel shows which is in use. Frames the encoder cannot keep up with
are dropped and counted rather than stalling the capture; the file is
playable even if the service is killed while recording. `ffmpeg` must be
installed. Scripts use `POST /api/record?start=1` and `?stop=1`.

### Network stream (for Frigate, go2rtc, another machine)

Off by default. Switched on from the panel or the tray, `serve` also serves
the picture as Motion JPEG on **port 8061 on all interfaces**:
`http://<host>:8061/stream.mjpg` (default 15 fps at 960x540; `?fps=30`,
`?scale=1` for full size, `?quality=90`), `http://<host>:8061/snapshot.jpg`
and a bare viewer page at `/`. It has no login by itself; `--stream-token SECRET` makes
every URL require `?token=SECRET`, otherwise use it on a trusted network
only. `--stream on` starts with it switched on,
`--stream-bind ADDR` moves it, `--stream-bind off` removes it, and
`--stream-fps N` / `--stream-scale N` change the defaults. Every client gets
its own encoder thread (a 960x540 JPEG costs about 14 ms of one core).

Frigate, through its bundled go2rtc, takes it like this:

```yaml
go2rtc:
  streams:
    hd60s:
      - "ffmpeg:http://buzzdeegaming:8061/stream.mjpg#video=h264#hardware"
cameras:
  hd60s:
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/hd60s
          roles: [detect, record]
```

or directly as an ffmpeg input (`- path: http://buzzdeegaming:8061/stream.mjpg`
with `input_args: -f mjpeg -re`) without go2rtc.

### Tray icon

`serve` also puts an icon into the system tray (a StatusNotifierItem over
D-Bus, so it shows in Plasma, in waybar under sway, and in other panels
that speak the protocol; `--tray off` disables it). The dot is grey without
a card, amber with a card but no HDMI signal, green while streaming; the
tooltip shows the input, frame rate and counters. Clicking it opens the
control panel; the menu offers mute, picture reset, the colour range and
quit. Without a session bus or a tray host the icon is simply not shown.

### Diagnostics

```bash
hd60s-linux report                # paste into an issue
hd60s-linux report --show-serial  # include the card's serial number
hd60s-linux report --no-capture   # skip the 3-second test capture
```

prints the tool and system versions, the device identity, the MCU status,
the EDID, the detected input and the settings, then streams for three
seconds and reports frame rate and errors. The panel's report button gives
the same while `serve` is running (with the live stream statistics instead
of a test capture).

## Capturing (Rev. 4)

Frames are written raw to stdout as YUYV 4:2:2. Whatever the source sends
(1080p60 down to 480p, see `docs/protocol.md`), every frame is placed centred
on a 1920x1080 canvas so the output keeps one size; `--native` writes frames
at source size instead and reports the geometry on stderr:

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

Without PipeWire, `tools/hd60s-obs` feeds video into a v4l2loopback device
(a kernel module) and audio into a PulseAudio/PipeWire sink instead;
`tools/hd60s-obs.service` keeps it running. See `docs/protocol.md` for the
module options.

Read the detected HDMI input timing, or watch it for a while:

```bash
cargo run --release -- signal      # once
cargo run --release -- signal 60   # watch, print changes
```

Show or change the HDMI colour range and the picture controls — the same
register writes the official application makes (bank `0x64`, registers
`0x12` and `0x13`); values are 0–255 with 128 as neutral:

```bash
cargo run --release -- picture
cargo run --release -- picture --range bypass --brightness 150
cargo run --release -- picture --reset
```

Audio gain (bank `0x64` register `0x3b`; 128 is 0 dB, about 0.5 dB per
step, 0 mutes):

```bash
cargo run --release -- audio
cargo run --release -- audio --gain 104   # about -12 dB
cargo run --release -- audio --mute
```

EDID — what the device tells its HDMI source it accepts (bank `0xa0`). Save
it, write a different one (validated; `--fix` recomputes checksums), or go
back to the power-on block; the source picks it up on the next hot-plug:

```bash
cargo run --release -- edid --dump current.bin
cargo run --release -- edid --write custom.bin
cargo run --release -- edid --restore
```

Microcontroller status, the three queries the official driver makes at
plug-in (nothing else can be sent through this path — see `docs/protocol.md`):

```bash
cargo run --release -- mcu
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

Stream the way the official application does — isochronous alternate
setting 2 after the register writes it makes — and dump the first 64 MB:

```bash
cargo run --release -- observe-iso 10
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
