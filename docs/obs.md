# OBS integration contract

The initial driver-to-OBS boundary is MPEG-TS over localhost UDP. This lets the
userspace driver deliver H.264 video and AAC audio without requiring a kernel
module or an OBS plugin.

## Validate with a test pattern

In OBS Studio:

1. Add a **Media Source**.
2. Clear **Local File**.
3. Set Input to `udp://127.0.0.1:5000?fifo_size=1000000&overrun_nonfatal=1`.
4. Set Input Format to `mpegts`.
5. Leave **Restart playback when source becomes active** enabled.

Then run:

```bash
./tools/obs-test-pattern
```

OBS should display a moving 1080p60 test pattern and play a 1 kHz tone. Stop
the generator with Ctrl+C. Only one receiver can bind UDP port 5000 at a time.

To test outside OBS, start the generator and then run:

```bash
ffplay -fflags nobuffer -flags low_delay \
  'udp://127.0.0.1:5000?fifo_size=1000000&overrun_nonfatal=1'
```

## Driver contract

The first capture-capable daemon will send:

- MPEG-TS packets in UDP datagrams with a 1316-byte payload;
- H.264 video in `yuv420p` with a two-second maximum keyframe interval;
- AAC-LC stereo audio at 48 kHz;
- monotonically increasing 90 kHz transport timestamps.

This is an integration contract, not evidence that HD60 S media has been
decoded. The test generator validates OBS independently while USB protocol work
continues.
