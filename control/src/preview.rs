//! The live picture: the service's YUYV frame to RGB, scaled with a box
//! filter so that Slint has nothing left to resample.

use slint::{Rgb8Pixel, SharedPixelBuffer};

use crate::{HEIGHT, WIDTH};

/// BT.709 limited range, integer arithmetic; `factor` > 1 averages
/// factor x factor source pixels (a box filter), which is what keeps the
/// scaled picture free of moiré.
pub(crate) fn yuyv_to_rgb(frame: &[u8], factor: usize) -> SharedPixelBuffer<Rgb8Pixel> {
    let (w, h) = (WIDTH / factor, HEIGHT / factor);
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(w as u32, h as u32);
    let pixels = buffer.make_mut_slice();
    let samples = (factor * factor) as i32;
    for y in 0..h {
        for x in 0..w {
            let (mut sy, mut su, mut sv) = (0_i32, 0_i32, 0_i32);
            for dy in 0..factor {
                let row = &frame[(y * factor + dy) * WIDTH * 2..];
                for dx in 0..factor {
                    let sx = x * factor + dx;
                    let pair = (sx / 2) * 4;
                    sy += row[pair + if sx.is_multiple_of(2) { 0 } else { 2 }] as i32;
                    su += row[pair + 1] as i32;
                    sv += row[pair + 3] as i32;
                }
            }
            let (yv, u, v) = (sy / samples, su / samples - 128, sv / samples - 128);
            let luma = (298 * (yv - 16)) >> 8;
            pixels[y * w + x] = Rgb8Pixel {
                r: (luma + ((459 * v) >> 8)).clamp(0, 255) as u8,
                g: (luma - ((55 * u + 136 * v) >> 8)).clamp(0, 255) as u8,
                b: (luma + ((541 * u) >> 8)).clamp(0, 255) as u8,
            };
        }
    }
    buffer
}
