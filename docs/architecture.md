# Architecture

A map for reading the code. The protocol itself is in `protocol.md`.

## Pieces

| Piece | Binary / crate | Job |
|---|---|---|
| Service | `hd60s-linux serve` (`src/serve.rs`) | Owns the card. Reads the USB stream, publishes a PipeWire camera and microphone, records, streams MJPEG, answers the API. |
| API | crate `hd60s-api` (`api/`) | The typed state (`State`, `Timing`, `Settings`, …), the commands, and the Unix-socket client. The only contract between the service and its clients. |
| Command line | `hd60s-linux` (`src/main.rs`) | `serve`, `ctl` (talks to the service), `report`, the direct commands (`signal`, `picture`, `audio`, `edid`, `mcu`) and the research tools. |
| Control program | `hd60s-control` (`control/`) | Slint window and tray icon; runs the service when nothing else does; autostart and udev access. |

## The service, thread by thread

```
USB reader ──chunks──▶ pump/assembler ──frames──▶ Shared.video.latest ──▶ PipeWire camera (vircam)
                                     ──audio──▶  Shared.audio ring    ──▶ PipeWire feed stream ──▶ virtual source
                                                 Shared.recorder      ──▶ ffmpeg child (Recording)
                                                 Shared.video.latest  ──▶ MJPEG clients (stream.rs), preview.jpg, frame.yuyv
API listeners (Unix socket, optional TCP) ──▶ panel.rs handlers ──▶ Shared, Card.control (registers)
```

- `pump.rs` — eight asynchronous 1 MB bulk transfers are always queued
  (`bulk_reader`); the completion callback resubmits each one. Decoding
  runs on the calling thread (`frame::Assembler`). The reader must never
  wait for anything: the card drops data during any gap.
- `serve.rs` — the pump thread finds the card, opens it, runs the plug-in
  sequence (`Control::initialise`), takes the MCU/EDID `Snapshot`, then
  streams; on loss it clears the shared state and retries every second.
  Deferred jobs that need the stream stopped (an EDID write) run between
  two attachments.
- `Shared` groups the state by concern: `video`, `audio`, `card`,
  `recorder`, `network`, `panel`.
- `panel.rs` builds the `State` for `GET /api/state` and applies the
  `POST`s; `http.rs` is the tiny HTTP server both listeners use.
- `record.rs` — ffmpeg child fed through bounded queues; `stream.rs` —
  one JPEG encoder per MJPEG client; `report.rs` — the diagnostic text.

## Iron rules (each cost a day)

1. **One USB handle per process.** Register access from a second handle
   while another handle streams hung the card until it was unplugged.
   The service uses its streaming handle for registers; the direct
   commands claim the interface first and refuse to run next to a service.
2. **Run the plug-in sequence before the microcontroller proxy.** From a
   true power-on state a bare proxy command hangs the card's USB
   controller. `Control::initialise` sends `0xec`, `0xc1`/c039, `0xc1`/4134
   and reads `0xc1`/0039 = 01, exactly as the official driver does.
3. **Microcontroller and EDID only before streaming.** The `Snapshot` is
   taken before the stream starts and shown afterwards.
4. **Never pause the USB reader.** Queued asynchronous transfers; every
   consumer (PipeWire, recorder, preview) is decoupled by a bounded queue
   or a "latest frame" slot and may lose frames, but never blocks the reader.
5. **Never send MCU command 0x60** (bootloader entry). `Control::mcu`
   allows only the three status commands.

## The control program

- `main.rs` — backend selection (Wayland app id), the `Runtime` shared
  with the threads, the state poll (1 s), the service/udev poll (5 s), the
  frame thread (~30 fps while a window exists), and the event loop.
- `ui.rs` — the window: built when shown, dropped when closed to the tray
  (a hidden window still counts as a running application on Wayland);
  `apply_state` maps `hd60s_api::State` onto the Slint properties.
- `service.rs` — systemd, the program's own `hd60s-linux serve` child
  (dies with the program), autostart choices, the udev access check and
  installer, the program's configuration file.
- `preview.rs` — YUYV to RGB with a box filter; `tray.rs` — the tray icon.

## Where things live at run time

| Path | What |
|---|---|
| `$XDG_RUNTIME_DIR/hd60s-linux/api.sock` | API socket (same user only) |
| `$XDG_RUNTIME_DIR/hd60s-linux/token` | token for the optional TCP panel |
| `~/.config/hd60s-linux/serve.conf` | `serve` defaults (`key = value`, flag names) |
| `~/.config/hd60s-linux/control.conf` | control program settings |
| `~/.config/autostart/hd60s-control.desktop` | "start with the desktop" |
| `~/Videos/HD60 S <date> <time>.mkv` | recordings |
