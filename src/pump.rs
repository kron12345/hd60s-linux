//! Turns an input — the device over USB, or a recorded raw stream — into
//! frames and audio, delivered through callbacks.
//!
//! The USB read runs on its own thread and never pauses, because the device
//! drops data during any gap between transfers; decoding happens on the
//! caller's thread. A recorded stream is replayed at a fixed frame rate so
//! that everything downstream behaves as it would with the device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::time::{Duration, Instant};

use rusb::{DeviceHandle, UsbContext};

use crate::frame::{Assembler, Event, Frame, Stats};

/// Where the raw stream comes from.
pub enum Input<T: UsbContext> {
    /// The device itself, already opened; interface 0 is claimed here. The
    /// handle is shared so the caller can keep using the control endpoint.
    Usb(Arc<DeviceHandle<T>>),
    /// A recorded raw bulk stream, replayed in a loop at `fps` frames per second.
    File { path: String, fps: f64 },
}

/// Runs until `stop` is set (or `seconds` have passed) and returns the
/// assembler's statistics. `on_frame` and `on_audio` are called on the
/// calling thread.
pub fn run<T: UsbContext + 'static>(
    input: Input<T>,
    stop: Arc<AtomicBool>,
    seconds: Option<u64>,
    on_frame: impl FnMut(Frame),
    on_audio: impl FnMut(Vec<u8>),
) -> Result<Stats, String> {
    run_with_stats(input, stop, seconds, None, on_frame, on_audio)
}

/// Like `run`, and additionally copies the running statistics into `live`
/// after every chunk, for a panel or a log to read while streaming.
pub fn run_with_stats<T: UsbContext + 'static>(
    input: Input<T>,
    stop: Arc<AtomicBool>,
    seconds: Option<u64>,
    live: Option<Arc<std::sync::Mutex<Stats>>>,
    mut on_frame: impl FnMut(Frame),
    mut on_audio: impl FnMut(Vec<u8>),
) -> Result<Stats, String> {
    let (sender, receiver) = sync_channel::<Vec<u8>>(256);
    let reader_stop = stop.clone();
    let paced = matches!(input, Input::File { .. });
    let fps = match &input {
        Input::File { fps, .. } => *fps,
        Input::Usb(_) => 0.0,
    };

    let (reader, usb_handle) = match input {
        Input::Usb(handle) => {
            let _ = handle.set_auto_detach_kernel_driver(true);
            handle
                .claim_interface(0)
                .map_err(|error| format!("claiming interface 0: {error}"))?;
            handle
                .set_alternate_setting(0, 4)
                .map_err(|error| format!("selecting interface 0 alternate setting 4: {error}"))?;
            let reader_handle = handle.clone();
            let reader = std::thread::spawn(move || -> Result<(), String> {
                let handle = reader_handle;
                let timeout = Duration::from_millis(200);
                let mut buffer = vec![0_u8; 1024 * 1024];
                while !reader_stop.load(Ordering::Relaxed) {
                    match handle.read_bulk(0x83, &mut buffer, timeout) {
                        Ok(length) => {
                            if sender.send(buffer[..length].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(rusb::Error::Timeout) => {}
                        Err(error) => return Err(format!("reading bulk endpoint 0x83: {error}")),
                    }
                }
                Ok(())
            });
            (reader, Some(handle))
        }
        Input::File { path, .. } => {
            let data = std::fs::read(&path).map_err(|error| format!("reading {path}: {error}"))?;
            if data.is_empty() {
                return Err(format!("{path} is empty"));
            }
            let reader = std::thread::spawn(move || -> Result<(), String> {
                let mut at = 0;
                while !reader_stop.load(Ordering::Relaxed) {
                    let end = (at + (1 << 20)).min(data.len());
                    if sender.send(data[at..end].to_vec()).is_err() {
                        break;
                    }
                    at = if end == data.len() { 0 } else { end };
                }
                Ok(())
            });
            (reader, None)
        }
    };

    let mut assembler = Assembler::new();
    let started = Instant::now();
    let limit = seconds.map(Duration::from_secs);
    let mut frames_emitted = 0_u64;

    while !stop.load(Ordering::Relaxed) {
        if let Some(limit) = limit
            && started.elapsed() >= limit
        {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                let mut emitted = 0_u64;
                assembler.push(&chunk, |event| match event {
                    Event::Frame(frame) => {
                        emitted += 1;
                        on_frame(frame);
                    }
                    Event::Audio(bytes) => on_audio(bytes),
                });
                frames_emitted += emitted;
                if let Some(live) = &live {
                    *live.lock().unwrap() = assembler.stats;
                }
                if paced && emitted > 0 {
                    // Hold the replay to real time: frame N is due at N / fps.
                    let due = Duration::from_secs_f64(frames_emitted as f64 / fps);
                    if let Some(wait) = due.checked_sub(started.elapsed()) {
                        std::thread::sleep(wait);
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    stop.store(true, Ordering::Relaxed);
    drop(receiver);
    let outcome = reader.join();
    // Hand the interface back so the same handle can stream again later.
    if let Some(handle) = usb_handle {
        let _ = handle.release_interface(0);
    }
    if let Ok(Err(error)) = outcome {
        return Err(error);
    }
    Ok(assembler.stats)
}
