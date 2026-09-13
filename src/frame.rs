//! Splits the raw HD60 S bulk stream into frames and embedded audio.
//!
//! Stream layout, measured on a Rev. 4 device across 1080p60/50/30, 720p60,
//! 1280x1024, 576p50 and 480p60 sources:
//!
//! ```text
//! <TRS ff 00 00 XY> <width*2 B pixels, YUYV> [<ff 00 ff N> <N stereo samples>]... <next TRS> ...
//! ```
//!
//! `XY` follows BT.656: bit 7 is always set, followed by F, V and H, and the
//! low four bits are protection bits derived from F/V/H. That parity reliably
//! separates real markers from a chance `ff 00 00` inside pixel data.
//!
//! A frame starts where V changes from 1 (vertical blanking) to 0. Its width
//! is the pixel payload of its lines, its height the number of active lines,
//! so the geometry follows the source without any configuration.

/// Width of the largest picture the device delivers.
pub const MAX_WIDTH: usize = 1920;
/// Height of the largest picture the device delivers.
pub const MAX_HEIGHT: usize = 1080;

const TRS_PREFIX: [u8; 3] = [0xff, 0x00, 0x00];
/// Audio trailer header: `ff 00 ff N`, followed by N stereo sample pairs of
/// 4 bytes. N is 2 on most lines; sources with fewer lines per frame than the
/// audio needs (720p60) also use 3.
const AUDIO_PREFIX: [u8; 3] = [0xff, 0x00, 0xff];
const MAX_AUDIO_PAIRS: usize = 16;

/// Splits the audio trailers off the end of a line. Returns the pixel length
/// and the trailers' payloads in stream order.
fn split_audio(line: &[u8]) -> (usize, Vec<&[u8]>) {
    let mut end = line.len();
    let mut blocks = Vec::new();
    'outer: loop {
        for pairs in 1..=MAX_AUDIO_PAIRS {
            let len = 4 + 4 * pairs;
            if end >= len
                && line[end - len..end - len + 3] == AUDIO_PREFIX
                && line[end - len + 3] as usize == pairs
            {
                blocks.push(&line[end - len + 4..end]);
                end -= len;
                continue 'outer;
            }
        }
        break;
    }
    blocks.reverse();
    (end, blocks)
}

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

/// One complete picture in YUYV.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

/// What the assembler extracts from the stream.
pub enum Event {
    Frame(Frame),
    /// Raw audio bytes, 16-bit little-endian, stereo, interleaved.
    Audio(Vec<u8>),
}

/// Counters for runtime diagnostics.
#[derive(Default, Clone, Copy)]
pub struct Stats {
    pub frames: u64,
    /// Frames whose lines disagreed on their width, that had no lines, or
    /// whose geometry matched neither the current format nor a new one seen
    /// twice in a row (a frame that lost lines to a USB drop).
    pub bad_frames: u64,
    pub audio_blocks: u64,
    pub unknown_blocks: u64,
    /// Frames whose geometry differed from the previous frame's.
    pub format_changes: u64,
}

pub struct Assembler {
    buffer: Vec<u8>,
    frame: Vec<u8>,
    synced: bool,
    prev_blanking: bool,
    active_lines: usize,
    line_bytes: Option<usize>,
    consistent: bool,
    last_geometry: Option<(usize, usize)>,
    candidate: Option<(usize, usize)>,
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
            frame: Vec::with_capacity(MAX_WIDTH * MAX_HEIGHT * 2),
            synced: false,
            prev_blanking: false,
            active_lines: 0,
            line_bytes: None,
            consistent: true,
            last_geometry: None,
            candidate: None,
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

    fn finish_frame(&mut self, emit: &mut impl FnMut(Event)) {
        let width = self.line_bytes.unwrap_or(0) / 2;
        let height = self.active_lines;
        let geometry = (width, height);
        // A geometry that differs from the current one is accepted only when
        // it shows up twice in a row; a single odd frame has lost lines.
        let accepted = match self.last_geometry {
            None => true,
            Some(last) if last == geometry => true,
            Some(_) if self.candidate == Some(geometry) => {
                self.stats.format_changes += 1;
                true
            }
            Some(_) => {
                self.candidate = Some(geometry);
                false
            }
        };
        if self.consistent && width > 0 && height > 0 && accepted {
            self.candidate = None;
            self.last_geometry = Some(geometry);
            self.stats.frames += 1;
            let pixels = std::mem::take(&mut self.frame);
            emit(Event::Frame(Frame {
                width,
                height,
                pixels,
            }));
            self.frame = Vec::with_capacity(width * height * 2);
        } else {
            self.stats.bad_frames += 1;
            self.frame.clear();
        }
        self.active_lines = 0;
        self.line_bytes = None;
        self.consistent = true;
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

            // A frame starts where vertical blanking ends.
            if self.prev_blanking && !blanking {
                if self.synced {
                    self.finish_frame(&mut emit);
                } else {
                    self.synced = true;
                    self.frame.clear();
                    self.active_lines = 0;
                    self.line_bytes = None;
                    self.consistent = true;
                }
            }

            // Audio trailers are appended to the line, after the pixels.
            let line = &self.buffer[cursor + 4..next];
            let (pixel_end, audio_blocks) = split_audio(line);
            for bytes in audio_blocks {
                self.stats.audio_blocks += 1;
                emit(Event::Audio(bytes.to_vec()));
            }
            let pixels = &line[..pixel_end];

            if self.synced && !blanking {
                match self.line_bytes {
                    None => self.line_bytes = Some(pixels.len()),
                    Some(expected) if expected != pixels.len() => {
                        self.consistent = false;
                        self.stats.unknown_blocks += 1;
                    }
                    Some(_) => {}
                }
                if self.consistent && self.active_lines < MAX_HEIGHT {
                    self.frame.extend_from_slice(pixels);
                    self.active_lines += 1;
                }
            }

            self.prev_blanking = blanking;
            cursor = next;
        }

        self.buffer.drain(..cursor);
    }
}

/// Places `frame` centred on a black canvas of `width` x `height` in YUYV.
/// A frame larger than the canvas is cropped around its centre; a frame of
/// exactly the canvas size is returned as it is.
pub fn letterbox(frame: Frame, width: usize, height: usize) -> Vec<u8> {
    if frame.width == width && frame.height == height {
        return frame.pixels;
    }
    // Black in YUYV is Y=0x10, C=0x80; fill by doubling instead of per pixel.
    let mut canvas = vec![0x10, 0x80];
    while canvas.len() < width * height * 2 {
        let len = canvas.len();
        canvas.extend_from_within(..len.min(width * height * 2 - len));
    }
    let copy_w = frame.width.min(width);
    let copy_h = frame.height.min(height);
    let src_x = ((frame.width - copy_w) / 2) & !1;
    let src_y = (frame.height - copy_h) / 2;
    let dst_x = ((width - copy_w) / 2) & !1;
    let dst_y = (height - copy_h) / 2;
    for row in 0..copy_h {
        let src = (src_y + row) * frame.width * 2 + src_x * 2;
        let dst = (dst_y + row) * width * 2 + dst_x * 2;
        canvas[dst..dst + copy_w * 2].copy_from_slice(&frame.pixels[src..src + copy_w * 2]);
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(width: usize, height: usize, blanking: usize, frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..frames {
            for _ in 0..blanking {
                out.extend_from_slice(&[0xff, 0x00, 0x00, 0xab]);
                out.extend(std::iter::repeat_n(0x10, width * 2));
            }
            for n in 0..height {
                out.extend_from_slice(&[0xff, 0x00, 0x00, 0x80]);
                out.extend(std::iter::repeat_n(0x42, width * 2));
                if n % 3 == 0 {
                    out.extend_from_slice(&[0xff, 0x00, 0xff, 0x02, 1, 0, 2, 0, 3, 0, 4, 0]);
                } else if n % 7 == 0 {
                    // Three-pair trailer as seen on 720p60 sources.
                    out.extend_from_slice(&[0xff, 0x00, 0xff, 0x03]);
                    out.extend_from_slice(&[5, 0, 6, 0, 7, 0, 8, 0, 9, 0, 10, 0]);
                }
            }
        }
        out
    }

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
    fn assembles_1080p_frames_and_audio() {
        let mut assembler = Assembler::new();
        let mut frames = 0;
        let mut audio = Vec::new();
        assembler.push(&stream(1920, 1080, 45, 3), |event| match event {
            Event::Frame(frame) => {
                assert_eq!((frame.width, frame.height), (1920, 1080));
                assert_eq!(frame.pixels.len(), 1920 * 1080 * 2);
                assert!(frame.pixels.iter().all(|&b| b == 0x42));
                frames += 1;
            }
            Event::Audio(bytes) => audio.extend_from_slice(&bytes),
        });
        // Three frames, two of them end at a blanking edge.
        assert_eq!(frames, 2);
        assert_eq!(&audio[..8], &[1, 0, 2, 0, 3, 0, 4, 0]);
        // Line 7 carries a three-pair trailer; it follows lines 0, 3 and 6.
        assert_eq!(&audio[24..36], &[5, 0, 6, 0, 7, 0, 8, 0, 9, 0, 10, 0]);
        assert_eq!(assembler.stats.bad_frames, 0);
        assert_eq!(assembler.stats.unknown_blocks, 0);
    }

    #[test]
    fn follows_the_source_geometry() {
        // 720p60 as measured: 720 active lines, 30 blanking, 2560 bytes per line.
        let mut assembler = Assembler::new();
        let mut seen = Vec::new();
        let mut data = stream(1280, 720, 30, 2);
        data.extend(stream(720, 576, 49, 3));
        assembler.push(&data, |event| {
            if let Event::Frame(frame) = event {
                seen.push((frame.width, frame.height));
            }
        });
        // The first 576p frame is held back as a candidate; the second confirms it.
        assert_eq!(seen, vec![(1280, 720), (1280, 720), (720, 576)]);
        assert_eq!(assembler.stats.format_changes, 1);
        assert_eq!(assembler.stats.bad_frames, 1);
    }

    #[test]
    fn a_single_short_frame_is_rejected_not_a_format_change() {
        let mut data = stream(1920, 1080, 45, 2);
        data.extend(stream(1920, 1060, 45, 1)); // lost lines
        data.extend(stream(1920, 1080, 45, 2));
        let mut assembler = Assembler::new();
        let mut seen = Vec::new();
        assembler.push(&data, |event| {
            if let Event::Frame(frame) = event {
                seen.push(frame.height);
            }
        });
        assert!(seen.iter().all(|&h| h == 1080), "{seen:?}");
        assert_eq!(assembler.stats.format_changes, 0);
        assert_eq!(assembler.stats.bad_frames, 1);
    }

    #[test]
    fn letterbox_centres_smaller_frames() {
        let frame = Frame {
            width: 4,
            height: 2,
            pixels: vec![0x42; 4 * 2 * 2],
        };
        let canvas = letterbox(frame, 8, 4);
        assert_eq!(canvas.len(), 8 * 4 * 2);
        // Row 1 (second row) holds the first source row, columns 2..6.
        let row = &canvas[8 * 2..8 * 2 * 2];
        assert_eq!(&row[..4], &[0x10, 0x80, 0x10, 0x80]);
        assert!(row[4..12].iter().all(|&b| b == 0x42));
        assert_eq!(&row[12..], &[0x10, 0x80, 0x10, 0x80]);
        // Top row stays black.
        assert_eq!(&canvas[..4], &[0x10, 0x80, 0x10, 0x80]);
    }
}
