//! Splits the raw HD60 S bulk stream into frames and embedded audio.
//!
//! Stream layout, measured on a Rev. 4 device:
//!
//! ```text
//! <TRS ff 00 00 XY> <3840 B pixels, YUYV> [<12 B audio block>] <next TRS> ...
//! ```
//!
//! `XY` follows BT.656: bit 7 is always set, followed by F, V and H, and the
//! low four bits are protection bits derived from F/V/H. That parity reliably
//! separates real markers from a chance `ff 00 00` inside pixel data.
//!
//! A frame spans 1125 lines, 1080 of them active. It starts where V changes
//! from 1 (vertical blanking) to 0.

pub const WIDTH: usize = 1920;
pub const HEIGHT: usize = 1080;
pub const LINE_BYTES: usize = WIDTH * 2;
pub const FRAME_BYTES: usize = HEIGHT * LINE_BYTES;

const TRS_PREFIX: [u8; 3] = [0xff, 0x00, 0x00];
const AUDIO_PREFIX: [u8; 4] = [0xff, 0x00, 0xff, 0x02];
const AUDIO_BLOCK: usize = 12;

/// Checks the BT.656 protection bits of a status byte.
fn valid_status(xy: u8) -> bool {
    if xy & 0x80 == 0 {
        return false;
    }
    let f = (xy >> 6) & 1;
    let v = (xy >> 5) & 1;
    let h = (xy >> 4) & 1;
    let guard = ((v ^ h) << 3) | ((f ^ h) << 2) | ((f ^ v) << 1) | (f ^ v ^ h);
    xy & 0x0f == guard
}

fn is_blanking(xy: u8) -> bool {
    (xy >> 5) & 1 == 1
}

/// What the assembler extracts from the stream.
pub enum Event {
    /// One complete picture, 1920x1080 in YUYV.
    Frame(Vec<u8>),
    /// Raw audio bytes, 16-bit little-endian, stereo, interleaved.
    Audio(Vec<u8>),
}

/// Counters for runtime diagnostics.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    pub short_frames: u64,
    pub audio_blocks: u64,
    pub unknown_blocks: u64,
    pub resyncs: u64,
}

pub struct Assembler {
    buffer: Vec<u8>,
    frame: Vec<u8>,
    synced: bool,
    prev_blanking: bool,
    active_lines: usize,
    pub stats: Stats,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::new()
    }
}

impl Assembler {
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(8 << 20),
            frame: Vec::with_capacity(FRAME_BYTES),
            synced: false,
            prev_blanking: false,
            active_lines: 0,
            stats: Stats::default(),
        }
    }

    /// Finds the next valid marker at or after `from`.
    fn next_trs(&self, from: usize) -> Option<usize> {
        let buf = &self.buffer;
        let mut i = from;
        while i + 4 <= buf.len() {
            let rel = memchr::memchr(TRS_PREFIX[0], &buf[i..buf.len() - 3])?;
            let at = i + rel;
            if buf[at + 1] == TRS_PREFIX[1]
                && buf[at + 2] == TRS_PREFIX[2]
                && valid_status(buf[at + 3])
            {
                return Some(at);
            }
            i = at + 1;
        }
        None
    }

    /// Consumes new bulk data and reports every completed event.
    pub fn push(&mut self, data: &[u8], mut emit: impl FnMut(Event)) {
        self.buffer.extend_from_slice(data);

        let mut cursor = match self.next_trs(0) {
            Some(at) => at,
            None => {
                // Nothing usable yet: keep the tail short, but never split a prefix.
                if self.buffer.len() > 1 << 22 {
                    let keep = self.buffer.len() - 3;
                    self.buffer.drain(..keep);
                }
                return;
            }
        };

        while let Some(next) = self.next_trs(cursor + 4) {
            let xy = self.buffer[cursor + 3];
            let blanking = is_blanking(xy);
            let body = &self.buffer[cursor + 4..next];

            // A frame starts where vertical blanking ends.
            if self.synced && self.prev_blanking && !blanking {
                if self.active_lines == HEIGHT {
                    self.stats.frames += 1;
                    emit(Event::Frame(std::mem::take(&mut self.frame)));
                } else {
                    self.stats.short_frames += 1;
                }
                self.frame = Vec::with_capacity(FRAME_BYTES);
                self.active_lines = 0;
            } else if !self.synced && self.prev_blanking && !blanking {
                self.synced = true;
                self.frame.clear();
                self.active_lines = 0;
            }

            // Excess bytes at the end of a line are the embedded audio block.
            let pixels = if body.len() > LINE_BYTES {
                let (head, tail) = body.split_at(LINE_BYTES);
                if tail.len() >= AUDIO_BLOCK && tail[..4] == AUDIO_PREFIX {
                    self.stats.audio_blocks += 1;
                    emit(Event::Audio(tail[4..AUDIO_BLOCK].to_vec()));
                } else {
                    self.stats.unknown_blocks += 1;
                }
                head
            } else {
                body
            };

            if self.synced && !blanking && self.active_lines < HEIGHT {
                self.frame.extend_from_slice(pixels);
                if pixels.len() < LINE_BYTES {
                    self.frame
                        .resize(self.frame.len() + LINE_BYTES - pixels.len(), 0);
                }
                self.active_lines += 1;
            }

            self.prev_blanking = blanking;
            cursor = next;
        }

        self.buffer.drain(..cursor);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt656_guard_bits_accept_observed_status_bytes() {
        // Both values occur on the hardware: active picture and blanking.
        assert!(valid_status(0x80));
        assert!(valid_status(0xab));
        assert!(is_blanking(0xab));
        assert!(!is_blanking(0x80));
    }

    #[test]
    fn bt656_guard_bits_reject_image_data() {
        // A chance ff 00 00 inside pixel data has no valid parity.
        assert!(!valid_status(0x00));
        assert!(!valid_status(0x7f));
        assert!(!valid_status(0x81));
        assert!(!valid_status(0xaa));
    }

    #[test]
    fn assembles_a_frame_from_synthetic_lines() {
        let mut stream = Vec::new();
        let line = |out: &mut Vec<u8>, xy: u8, fill: u8| {
            out.extend_from_slice(&[0xff, 0x00, 0x00, xy]);
            out.extend(std::iter::repeat_n(fill, LINE_BYTES));
        };
        // Three frames, so that two of them end at a blanking edge.
        for _ in 0..3 {
            for _ in 0..45 {
                line(&mut stream, 0xab, 0x10);
            }
            for _ in 0..HEIGHT {
                line(&mut stream, 0x80, 0x42);
            }
        }

        let mut assembler = Assembler::new();
        let mut frames = 0;
        assembler.push(&stream, |event| {
            if let Event::Frame(frame) = event {
                assert_eq!(frame.len(), FRAME_BYTES);
                assert!(frame.iter().all(|&b| b == 0x42));
                frames += 1;
            }
        });
        assert_eq!(frames, 2, "expected two completed frames");
        assert_eq!(assembler.stats.short_frames, 0);
    }

    #[test]
    fn extracts_embedded_audio_from_overlong_lines() {
        let mut stream = Vec::new();
        for _ in 0..2 {
            for _ in 0..45 {
                stream.extend_from_slice(&[0xff, 0x00, 0x00, 0xab]);
                stream.extend(std::iter::repeat_n(0x10, LINE_BYTES));
            }
            for n in 0..HEIGHT {
                stream.extend_from_slice(&[0xff, 0x00, 0x00, 0x80]);
                stream.extend(std::iter::repeat_n(0x42, LINE_BYTES));
                if n % 3 == 0 {
                    stream.extend_from_slice(&AUDIO_PREFIX);
                    stream.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
                }
            }
        }

        let mut assembler = Assembler::new();
        let mut audio = Vec::new();
        assembler.push(&stream, |event| {
            if let Event::Audio(bytes) = event {
                audio.extend_from_slice(&bytes);
            }
        });
        assert!(!audio.is_empty(), "expected audio blocks");
        assert_eq!(audio.len() % 8, 0);
        assert_eq!(&audio[..8], &[1, 0, 2, 0, 3, 0, 4, 0]);
        assert_eq!(assembler.stats.unknown_blocks, 0);
    }
}
