# Roadmap

What works is in the README; this is what is open.

- **Interlaced sources** (1080i from cameras such as the EOS 700D): the F
  bit is parsed but fields are not yet woven; needs a real source.
- **Rev. 1 to 3**: the development Rev. 2 unit returned no stream data;
  whether they need an enable step is unknown. A trace from such a unit
  under the official driver would settle it.
- **HDCP-protected sources**: unmeasured.
- **Interrupt endpoint `0x81`** stayed silent everywhere; the official
  driver disarms event reporting (`0xc6`), so it may simply never speak.
- Meaning of the `0xec` payload and of requests `0xc2` and `0xc7`.
- Debian package: builds, not yet tested on a Debian system. An AUR entry.
- A native window icon on X11 (the Wayland app id is set).
