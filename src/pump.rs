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

use std::sync::mpsc::{SyncSender, TrySendError};

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
                bulk_reader(&reader_handle, sender, reader_stop)
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

/// Transfers kept in flight on the bulk endpoint. The device drops data
/// during any gap between transfers, so several are always queued and each
/// is resubmitted from its completion callback; nothing that happens on
/// other threads (register reads, a slow consumer) can then open a gap.
const BULK_TRANSFERS: usize = 8;
const BULK_TRANSFER_BYTES: usize = 1 << 20;

struct BulkState {
    sender: SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
    error: Option<String>,
    in_flight: usize,
    dropped: u64,
}

extern "system" fn bulk_callback(transfer: *mut libusb1_sys::libusb_transfer) {
    // SAFETY: the transfer and its user data were created in `bulk_reader`
    // and stay alive until every transfer has been freed there.
    unsafe {
        let state = &mut *((*transfer).user_data as *mut BulkState);
        state.in_flight -= 1;
        use libusb1_sys::constants::*;
        match (*transfer).status {
            LIBUSB_TRANSFER_COMPLETED | LIBUSB_TRANSFER_TIMED_OUT => {
                let got = (*transfer).actual_length as usize;
                if got > 0 {
                    let chunk = std::slice::from_raw_parts((*transfer).buffer, got).to_vec();
                    match state.sender.try_send(chunk) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) => state.dropped += 1,
                        Err(TrySendError::Disconnected(_)) => {
                            state.stop.store(true, Ordering::Relaxed);
                        }
                    }
                }
            }
            LIBUSB_TRANSFER_CANCELLED => return,
            LIBUSB_TRANSFER_NO_DEVICE => {
                state.error = Some(
                    "reading bulk endpoint 0x83: No such device (it may have been disconnected)"
                        .into(),
                );
                return;
            }
            other => {
                state.error = Some(format!(
                    "reading bulk endpoint 0x83: transfer status {other}"
                ));
                return;
            }
        }
        if state.stop.load(Ordering::Relaxed) || state.error.is_some() {
            return;
        }
        if libusb1_sys::libusb_submit_transfer(transfer) == 0 {
            state.in_flight += 1;
        } else {
            state.error = Some("resubmitting a bulk transfer failed".into());
        }
    }
}

fn bulk_reader<T: UsbContext>(
    handle: &DeviceHandle<T>,
    sender: SyncSender<Vec<u8>>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut state = Box::new(BulkState {
        sender,
        stop: stop.clone(),
        error: None,
        in_flight: 0,
        dropped: 0,
    });
    let mut buffers: Vec<Vec<u8>> = (0..BULK_TRANSFERS)
        .map(|_| vec![0_u8; BULK_TRANSFER_BYTES])
        .collect();
    let mut transfers = Vec::with_capacity(BULK_TRANSFERS);
    let context = handle.context().as_raw();
    let wait = libc::timeval {
        tv_sec: 0,
        tv_usec: 100_000,
    };
    // SAFETY: plain libusb asynchronous API; every pointer handed to libusb
    // outlives the transfers, which are cancelled and freed below.
    unsafe {
        for buffer in buffers.iter_mut() {
            let transfer = libusb1_sys::libusb_alloc_transfer(0);
            if transfer.is_null() {
                state.error = Some("allocating a bulk transfer".into());
                break;
            }
            (*transfer).dev_handle = handle.as_raw();
            (*transfer).endpoint = 0x83;
            (*transfer).transfer_type = libusb1_sys::constants::LIBUSB_TRANSFER_TYPE_BULK;
            (*transfer).timeout = 1000;
            (*transfer).buffer = buffer.as_mut_ptr();
            (*transfer).length = buffer.len() as i32;
            (*transfer).num_iso_packets = 0;
            (*transfer).callback = bulk_callback;
            (*transfer).user_data = &mut *state as *mut BulkState as *mut std::ffi::c_void;
            transfers.push(transfer);
            let rc = libusb1_sys::libusb_submit_transfer(transfer);
            if rc != 0 {
                state.error = Some(format!("submitting a bulk transfer: libusb error {rc}"));
                break;
            }
            state.in_flight += 1;
        }
        while !stop.load(Ordering::Relaxed) && state.error.is_none() {
            libusb1_sys::libusb_handle_events_timeout_completed(
                context,
                &wait,
                std::ptr::null_mut(),
            );
        }
        for transfer in &transfers {
            libusb1_sys::libusb_cancel_transfer(*transfer);
        }
        let mut rounds = 0;
        while state.in_flight > 0 && rounds < 30 {
            libusb1_sys::libusb_handle_events_timeout_completed(
                context,
                &wait,
                std::ptr::null_mut(),
            );
            rounds += 1;
        }
        for transfer in transfers {
            libusb1_sys::libusb_free_transfer(transfer);
        }
    }
    if state.dropped > 0 {
        eprintln!(
            "USB reader: {} chunk(s) dropped because the consumer fell behind",
            state.dropped
        );
    }
    match state.error.take() {
        Some(error) => Err(error),
        None => Ok(()),
    }
}
