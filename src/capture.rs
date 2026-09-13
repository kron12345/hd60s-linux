//! `hd60s-linux capture`: raw YUYV frames to stdout (and audio to a file),
//! for ffmpeg pipelines and for recording fixtures.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use rusb::{Device, UsbContext};

use crate::frame;

pub fn capture<T: UsbContext + 'static>(
    device: Device<T>,
    seconds: Option<u64>,
    audio_path: Option<&str>,
    native: bool,
) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    let audio_file = match audio_path {
        Some(path) => {
            Some(File::create(path).map_err(|error| format!("creating {path}: {error}"))?)
        }
        None => None,
    };

    // Writers run on their own threads with bounded queues, so a slow or
    // stalled consumer costs frames but never blocks decoding. Blocking here
    // would stop the audio and video outputs together and can deadlock a
    // consumer that waits for one before reading the other.
    let write_error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let (frame_sender, frame_receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(3);
    let frame_error = write_error.clone();
    let frame_writer = std::thread::spawn(move || {
        let mut stdout = std::io::BufWriter::with_capacity(
            frame::MAX_WIDTH * frame::MAX_HEIGHT * 2,
            std::io::stdout(),
        );
        for pixels in frame_receiver {
            if let Err(error) = stdout.write_all(&pixels).and_then(|()| stdout.flush()) {
                *frame_error.lock().unwrap() = Some(format!("writing frame to stdout: {error}"));
                break;
            }
        }
    });
    let (audio_sender, audio_receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(1024);
    let audio_error = write_error.clone();
    let audio_writer = std::thread::spawn(move || {
        let Some(mut file) = audio_file else { return };
        for bytes in audio_receiver {
            if let Err(error) = file.write_all(&bytes) {
                *audio_error.lock().unwrap() = Some(format!("writing audio: {error}"));
                break;
            }
        }
    });

    if native {
        eprintln!("capturing yuyv422 at source size from bulk endpoint 0x83");
    } else {
        eprintln!(
            "capturing yuyv422 on a {}x{} canvas from bulk endpoint 0x83",
            frame::MAX_WIDTH,
            frame::MAX_HEIGHT
        );
    }

    // The pump reads with queued asynchronous transfers (the hardware
    // discards data during any gap) and decodes on this thread.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let live = std::sync::Arc::new(std::sync::Mutex::new(frame::Stats::default()));
    let started = Instant::now();
    let mut reported = Instant::now();
    let mut geometry: Option<(usize, usize)> = None;
    let dropped_frames = std::cell::Cell::new(0_u64);
    let dropped_audio = std::cell::Cell::new(0_u64);
    let result = crate::pump::run_with_stats(
        crate::pump::Input::Usb(std::sync::Arc::new(handle)),
        stop.clone(),
        seconds,
        Some(live.clone()),
        |frame| {
            if geometry != Some((frame.width, frame.height)) {
                eprintln!("source: {}x{}", frame.width, frame.height);
                geometry = Some((frame.width, frame.height));
            }
            let pixels = if native {
                frame.pixels
            } else {
                frame::letterbox(frame, frame::MAX_WIDTH, frame::MAX_HEIGHT)
            };
            if frame_sender.try_send(pixels).is_err() {
                dropped_frames.set(dropped_frames.get() + 1);
            }
            if let Some(error) = write_error.lock().unwrap().take() {
                // Consumer gone (for example FFmpeg exited): a normal end.
                eprintln!("stopping: {error}");
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if reported.elapsed() >= Duration::from_secs(5) {
                let stats = *live.lock().unwrap();
                let elapsed = started.elapsed().as_secs_f64();
                eprintln!(
                    "{} frame(s), {:.1} fps, {} audio block(s), {} bad, {} unknown, {} format change(s), {}/{} dropped by consumer",
                    stats.frames,
                    stats.frames as f64 / elapsed,
                    stats.audio_blocks,
                    stats.bad_frames,
                    stats.unknown_blocks,
                    stats.format_changes,
                    dropped_frames.get(),
                    dropped_audio.get()
                );
                reported = Instant::now();
            }
        },
        |bytes| {
            if audio_path.is_some() && audio_sender.try_send(bytes).is_err() {
                dropped_audio.set(dropped_audio.get() + 1);
            }
        },
    );
    let elapsed = started.elapsed().as_secs_f64();
    drop(frame_sender);
    drop(audio_sender);
    // The frame writer may sit in a write to a stalled consumer; every frame
    // is flushed as it is written, so there is nothing to wait for.
    drop(frame_writer);
    let _ = audio_writer.join();
    let (dropped_frames, dropped_audio) = (dropped_frames.get(), dropped_audio.get());
    if dropped_frames > 0 || dropped_audio > 0 {
        eprintln!(
            "{dropped_frames} frame(s) and {dropped_audio} audio block(s) dropped by a slow consumer"
        );
    }
    let stats = result?;
    eprintln!(
        "capture ended: {} frame(s) in {:.1} s ({:.2} fps), {} audio block(s), {} bad frame(s)",
        stats.frames,
        elapsed,
        stats.frames as f64 / elapsed,
        stats.audio_blocks,
        stats.bad_frames
    );
    Ok(())
}
