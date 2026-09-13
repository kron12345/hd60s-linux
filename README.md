# hd60s-linux

Native Linux support for the **original** Elgato Game Capture HD60 S — a
userspace driver, no kernel module, nothing running as root. (The HD60 S+
is different hardware and out of scope.)

The official Windows driver serves four hardware revisions: `0fd9:004f`,
`0fd9:005e` (Rev. 2), `0fd9:0074` (Rev. 3) and `0fd9:0076` (Rev. 4). All
four are recognised. **Rev. 4 is measured and works** — 1080p60 video and
48 kHz stereo audio, every input format the card accepts, picture
controls, EDID, recording. The Rev. 2 unit the project started on returned
no stream data under the same sequence, so the older revisions are not yet
confirmed; reports welcome. Everything here is documented from
measurements (`docs/protocol.md`), not guessed. This project never touches
the card's firmware.

## Install

Arch Linux and derivatives, from `packaging/`:

```bash
cd packaging && makepkg -si
```

Debian/Ubuntu, with [cargo-deb](https://github.com/kornelski/cargo-deb):

```bash
cargo install cargo-deb
cargo build --release --workspace && cargo deb --no-build
sudo apt install ./target/debian/hd60s-linux_*.deb
```

The package installs the two programs, a udev rule that lets the logged-in
user open the card (the only step that needs root, and the package does
it), a desktop entry and an optional systemd user unit. Nothing starts by
itself. `ffmpeg` is needed for recording.

## Use it

Open **HD60 S Control** from the application menu (or run `hd60s-control`).
It starts the capture service, and from then on OBS (Sources → Video
Capture Device (PipeWire) and Audio Input Capture), browsers through the
camera portal, `gst-launch-1.0 pipewiresrc target-object="Elgato HD60 S"`
and every other PipeWire client see a camera **"Elgato HD60 S"** and a
microphone **"Elgato HD60 S Audio"**. Frames are always 1920x1080 YUYV
(smaller sources are centred on a black canvas), audio 48 kHz stereo.

The window shows the live picture, the detected input, the stream
statistics, and lets you set brightness, contrast, saturation, hue, the
colour range and the audio gain; it shows the device, its EDID and the
microcontroller status, records to a file, switches the network stream,
and produces a report for bug reports. Closing the window keeps the
program in the tray (grey: no card, amber: no signal, green: streaming;
the tray menu has record, mute and quit); `hd60s-control --tray` starts
minimised.

The *Service* box decides how the service runs:

- **Autostart: none** — the service runs while the program is open;
  *Quit* in the tray releases the card.
- **with the desktop, minimised to the tray** — an XDG autostart entry
  (`~/.config/autostart/hd60s-control.desktop`); sway users add
  `exec hd60s-control --tray` to their config instead.
- **systemd user service in the background** — `hd60s-serve.service`
  runs at login and with the card even without the program; the choice
  for headless boxes.

If a card is plugged in but your user may not open it, the box says so
and offers to install the udev rule (through polkit; without
administrator rights it saves the rule in `~/.config/hd60s-linux/` with
the command an administrator needs). Uninstalling leaves nothing running.

## Recording

The record button (window or tray) writes H.264 + AAC into
`~/Videos/HD60 S <date> <time>.mkv` through `ffmpeg`. The encoder is
chosen automatically — the GPU's VA-API encoder when a test encode
succeeds, libx264 otherwise. Frames the encoder cannot keep up with are
dropped and counted rather than stalling the capture; the file stays
playable if the service is killed while recording.

## Network stream (Frigate, go2rtc, another machine)

Off by default; switched on from the window or the tray. The service then
serves Motion JPEG on **port 8061 on all interfaces**:
`http://<host>:8061/stream.mjpg` (15 fps at 960x540 by default; `?fps=30`,
`?scale=1` for full size, `?quality=90`), `http://<host>:8061/snapshot.jpg`,
and a bare viewer page at `/`. There is no login by itself —
`stream-token = SECRET` (see the configuration below) makes every URL
require `?token=SECRET`; otherwise use it on a trusted network only.
Every client gets its own encoder (a 960x540 JPEG costs about 14 ms of
one core).

Frigate, through its bundled go2rtc:

```yaml
go2rtc:
  streams:
    hd60s:
      - "ffmpeg:http://<host>:8061/stream.mjpg#video=h264#hardware"
cameras:
  hd60s:
    ffmpeg:
      inputs:
        - path: rtsp://127.0.0.1:8554/hd60s
          roles: [detect, record]
```

or directly as an ffmpeg input (`- path: http://<host>:8061/stream.mjpg`
with `input_args: -f mjpeg -re`).

## The service, the command line and headless use

`hd60s-linux serve` is the service the program runs. It needs no desktop,
only a PipeWire user session; on a headless box let the user's services
run without a login and enable the unit:

```bash
sudo loginctl enable-linger "$USER"
systemctl --user enable --now hd60s-serve.service
```

Defaults go into `~/.config/hd60s-linux/serve.conf` (`key = value`, the
flag names; flags on the command line win):

```ini
panel = on            # web panel and HTTP API on 127.0.0.1:8060
stream = on           # MJPEG for Frigate/go2rtc on 0.0.0.0:8061
stream-token = secret
record-dir = /srv/recordings
record-encoder = vaapi
```

All `serve` options: `--name NAME`, `--panel on|ADDR`, `--record-dir DIR`,
`--record-encoder auto|x264|vaapi`, `--stream on`, `--stream-bind ADDR|off`,
`--stream-fps N`, `--stream-scale N`, `--stream-token T`,
`--from-file RAW [--fps N]` (replays a recorded raw stream instead of the
card, for development without hardware).

The running service is driven with `hd60s-linux ctl` (over its Unix
socket, same user only, no token):

```bash
hd60s-linux ctl status
hd60s-linux ctl picture --brightness 140 --range bypass
hd60s-linux ctl mute            # gain 0; unmute = gain 128
hd60s-linux ctl record start    # ... ctl record stop
hd60s-linux ctl stream on
hd60s-linux ctl snapshot now.jpg
hd60s-linux ctl edid dump card.bin
hd60s-linux ctl report
hd60s-linux ctl json            # the whole state for scripts
```

`hd60s-linux report` prints diagnostics for an issue (tool and system
versions, device, MCU status, EDID, input, settings, then a three-second
test capture; `--show-serial` adds the serial number).

### Web panel

With `panel = on` (or `--panel on|ADDR`) the service also serves a web
version of the control program on <http://127.0.0.1:8060/>, for scripts
or a browser on another machine through an SSH tunnel. Reading is open to
anything on the machine; **every change needs a token** that is new at
each start, embedded in the page and written to
`$XDG_RUNTIME_DIR/hd60s-linux/token` (mode 0600):

```bash
curl -X POST -H "X-Token: $(cat "$XDG_RUNTIME_DIR/hd60s-linux/token")" \
  "http://127.0.0.1:8060/api/record?start=1"
```

A web page you happen to visit cannot use it: it does not know the token,
the custom header forces a CORS preflight the panel refuses, a foreign
`Origin` and a `Host` other than localhost (DNS rebinding) are rejected.
The JSON API is the same one the program and `ctl` use: `GET /api/state`,
`POST /api/set?brightness=…|range=…|gain=…|reset=1`, `/api/record`,
`/api/stream`, `/api/edid?restore=1`, `GET /preview.jpg`, `/frame.yuyv`,
`/edid.bin`, `/api/report`.

## Direct access to the card

These commands open the card themselves and therefore refuse to run while
the service streams (stop it, or use `ctl`). Every one first sends the
official driver's plug-in sequence — without it a microcontroller query
hangs the card until it is unplugged (`docs/protocol.md`).

```bash
hd60s-linux signal [SECONDS]   # detected input timing, once or watched
hd60s-linux picture [--range bypass|shrink|expand] [--brightness N]
                    [--contrast N] [--saturation N] [--hue N] [--reset]
hd60s-linux audio [--gain N|--mute]        # 128 = 0 dB, ~0.5 dB per step
hd60s-linux edid [--dump FILE] [--write FILE [--fix]] [--restore]
hd60s-linux mcu                            # the three status queries
hd60s-linux capture [SECONDS] [--audio FILE] [--native]
```

`capture` writes raw YUYV frames to stdout (letterboxed to 1920x1080, or
at source size with `--native`) and the audio as `s16le` stereo 48 kHz:

```bash
hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - out.mkv
# or into a v4l2loopback device for software without PipeWire:
hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - -f v4l2 /dev/video10
```

The EDID commands change what the card tells its HDMI source it accepts;
the source re-reads it on the next hot-plug. `edid --restore` returns to
the power-on block.

## Research tools

`hd60s-linux inspect` prints the USB descriptors; `observe`,
`observe-stream` and `observe-iso [SECONDS]` watch the interrupt endpoint,
the bulk stream and the official isochronous path; `startup-status` is the
class/interface read from the static driver analysis. `docs/tracing.md`
describes capturing the Windows driver through QEMU with
`tools/capture-usbmon`, `tools/decode-usb-trace` and the
`analyze-usb-trace` binary. `cargo test --workspace` runs the tests.

## Documentation

- [docs/architecture.md](docs/architecture.md) — the map of the code, the
  threads and the rules that must not be broken.
- [docs/protocol.md](docs/protocol.md) — everything measured about the
  card: framing, audio, registers, the plug-in sequence, the MCU.
- [docs/vendor-driver-analysis.md](docs/vendor-driver-analysis.md) — facts
  from the official driver; [docs/tracing.md](docs/tracing.md) — how the
  traces were taken; [docs/roadmap.md](docs/roadmap.md) — what is open.

## Legal and safety boundaries

Contributors must capture traffic from hardware and software they are
authorised to use. Do not commit Elgato firmware, Windows drivers, HDCP
keys, device serial numbers, or copyrighted binary blobs. Capture fixtures
must be sanitised before publication. The tool never writes the card's
firmware or flash and never sends the microcontroller's bootloader command.

## License

MIT
