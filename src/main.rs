use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::{fs::File, io::Write};

use hd60s_linux::control::Control;
use hd60s_linux::device::{known_ids, revision as hd60s_revision};
use hd60s_linux::edid;
use hd60s_linux::frame::{self, Assembler, Event};
use hd60s_linux::serve::{Source, serve};
use rusb::{
    Context, Device, DeviceDescriptor, Direction, Recipient, RequestType, TransferType, UsbContext,
};

const REGISTER_REQUEST: u8 = 0xc0;
const HDMI_REGISTER_BANK: u16 = 0x0098;
const STARTUP_STATUS_REGISTER: u16 = 0x003b;
/// Rev. 4 register bank the official driver polls for the input timing.
const SIGNAL_REGISTER_BANK: u16 = 0x0064;
/// Bank 0x64 register for the colour range conversion: 0 bypass, 1 shrink, 2 expand.
const COLOUR_RANGE_REGISTER: u16 = 0x0012;
/// Bank 0x64 register holding brightness, contrast, saturation and hue,
/// one byte each with 0x80 as neutral.
const PICTURE_REGISTER: u16 = 0x0013;
/// Bank 0x64 register for the audio gain: 0x00 mutes, 0x80 is 0 dB, and
/// each step is about 0.5 dB (measured -32 dB at 0x40, -16 dB at 0x60).
const AUDIO_GAIN_REGISTER: u16 = 0x003b;
/// Bank holding the 256-byte EDID the device presents to its HDMI source; it
/// is an EEPROM behind the device's microcontroller, accessed in 16-byte pieces.
const EDID_BANK: u16 = 0x00a0;

fn transfer_name(transfer_type: TransferType) -> &'static str {
    match transfer_type {
        TransferType::Control => "control",
        TransferType::Isochronous => "isochronous",
        TransferType::Bulk => "bulk",
        TransferType::Interrupt => "interrupt",
    }
}

fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::In => "in",
        Direction::Out => "out",
    }
}

fn inspect_device<T: UsbContext>(
    device: Device<T>,
    descriptor: DeviceDescriptor,
    show_serial: bool,
) -> rusb::Result<()> {
    println!(
        "{} {:04x}:{:04x} at bus {:03} address {:03}",
        hd60s_revision(descriptor.vendor_id(), descriptor.product_id()).unwrap_or("HD60 S"),
        descriptor.vendor_id(),
        descriptor.product_id(),
        device.bus_number(),
        device.address()
    );
    println!("USB version: {}", descriptor.usb_version());
    println!("device version: {}", descriptor.device_version());
    println!("configurations: {}", descriptor.num_configurations());

    if let Ok(handle) = device.open() {
        let timeout = Duration::from_secs(1);
        if let Ok(languages) = handle.read_languages(timeout)
            && let Some(language) = languages.first().copied()
        {
            if let Ok(value) = handle.read_manufacturer_string(language, &descriptor, timeout) {
                println!("manufacturer: {value}");
            }
            if let Ok(value) = handle.read_product_string(language, &descriptor, timeout) {
                println!("product: {value}");
            }
            if show_serial
                && let Ok(value) = handle.read_serial_number_string(language, &descriptor, timeout)
            {
                println!("serial: {value}");
            }
        }
    } else {
        eprintln!("warning: cannot open device; descriptors may still be inspected");
    }

    for config_index in 0..descriptor.num_configurations() {
        let config = device.config_descriptor(config_index)?;
        println!(
            "configuration {}: {} interface(s), {} mA",
            config.number(),
            config.num_interfaces(),
            config.max_power()
        );
        for interface in config.interfaces() {
            for alt in interface.descriptors() {
                println!(
                    "  interface {} alt {}: class {:02x}/{:02x}/{:02x}",
                    alt.interface_number(),
                    alt.setting_number(),
                    alt.class_code(),
                    alt.sub_class_code(),
                    alt.protocol_code()
                );
                for endpoint in alt.endpoint_descriptors() {
                    println!(
                        "    endpoint 0x{:02x}: {:11} {:3}, max packet {}, interval {}",
                        endpoint.address(),
                        transfer_name(endpoint.transfer_type()),
                        direction_name(endpoint.direction()),
                        endpoint.max_packet_size(),
                        endpoint.interval()
                    );
                }
            }
        }
    }

    Ok(())
}

fn observe_interrupt<T: UsbContext>(device: Device<T>, seconds: u64) -> rusb::Result<()> {
    eprintln!("stage: opening device");
    let handle = device.open()?;
    eprintln!("stage: device open");
    match handle.kernel_driver_active(1) {
        Ok(active) => eprintln!("stage: interface 1 kernel driver active: {active}"),
        Err(error) => eprintln!("stage: kernel-driver query unavailable: {error}"),
    }
    eprintln!("stage: claiming interface 1");
    handle.claim_interface(1)?;
    eprintln!("stage: interface 1 claimed");

    let started = Instant::now();
    let duration = Duration::from_secs(seconds);
    let timeout = Duration::from_millis(200);
    let mut buffer = [0_u8; 64];
    let mut packets = 0_u64;

    println!("observing interrupt endpoint 0x81 for {seconds} seconds");
    while started.elapsed() < duration {
        match handle.read_interrupt(0x81, &mut buffer, timeout) {
            Ok(length) => {
                packets += 1;
                let hex = buffer[..length]
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<Vec<_>>()
                    .join(" ");
                println!(
                    "{:>8.3}s  {:>2} bytes  {hex}",
                    started.elapsed().as_secs_f64(),
                    length
                );
            }
            Err(rusb::Error::Timeout) => {}
            Err(error) => return Err(error),
        }
    }
    println!("observation complete: {packets} packet(s)");
    Ok(())
}

fn observe_stream<T: UsbContext>(device: Device<T>, seconds: u64) -> Result<(), String> {
    eprintln!("stage: opening device");
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    // A kernel driver bound to the interface (for example hd60s.ko) is
    // detached first; libusb rebinds nothing, so it stays off until a replug.
    let _ = handle.set_auto_detach_kernel_driver(true);
    eprintln!("stage: claiming interface 0");
    handle
        .claim_interface(0)
        .map_err(|error| format!("claiming interface 0: {error}"))?;
    eprintln!("stage: selecting bulk alternate setting 4");
    handle
        .set_alternate_setting(0, 4)
        .map_err(|error| format!("selecting interface 0 alternate setting 4: {error}"))?;

    let path = "/tmp/hd60s-stream.bin";
    let mut output = File::create(path).map_err(|error| format!("creating {path}: {error}"))?;
    let started = Instant::now();
    let duration = Duration::from_secs(seconds);
    let timeout = Duration::from_millis(200);
    let mut buffer = vec![0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    let mut transfers = 0_u64;

    println!("observing bulk endpoint 0x83 for {seconds} seconds");
    while started.elapsed() < duration {
        match handle.read_bulk(0x83, &mut buffer, timeout) {
            Ok(length) => {
                output
                    .write_all(&buffer[..length])
                    .map_err(|error| format!("writing {path}: {error}"))?;
                bytes += length as u64;
                transfers += 1;
            }
            Err(rusb::Error::Timeout) => {}
            Err(error) => return Err(format!("reading bulk endpoint 0x83: {error}")),
        }
    }
    println!("observation complete: {transfers} transfer(s), {bytes} byte(s), output {path}");
    Ok(())
}

/// Reads the bulk stream, assembles frames and writes them raw to stdout.
///
/// The output is YUYV 4:2:2. By default every frame is placed centred on a
/// 1920x1080 canvas, so the stream keeps one size whatever the source does;
/// `--native` writes frames at the source size instead and reports changes.
/// `hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - ...`
/// With `--audio FILE` the embedded audio bytes are written there as well
/// (16-bit little-endian, stereo, 48 kHz).
fn capture<T: UsbContext + 'static>(
    device: Device<T>,
    seconds: Option<u64>,
    audio_path: Option<&str>,
    native: bool,
) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    // A kernel driver bound to the interface (for example hd60s.ko) is
    // detached first; libusb rebinds nothing, so it stays off until a replug.
    let _ = handle.set_auto_detach_kernel_driver(true);
    handle
        .claim_interface(0)
        .map_err(|error| format!("claiming interface 0: {error}"))?;
    handle
        .set_alternate_setting(0, 4)
        .map_err(|error| format!("selecting interface 0 alternate setting 4: {error}"))?;

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
    let mut dropped_frames = 0_u64;
    let mut dropped_audio = 0_u64;
    let mut assembler = Assembler::new();
    let started = Instant::now();
    let limit = seconds.map(Duration::from_secs);
    let mut reported = Instant::now();

    if native {
        eprintln!("capturing yuyv422 at source size from bulk endpoint 0x83");
    } else {
        eprintln!(
            "capturing yuyv422 on a {}x{} canvas from bulk endpoint 0x83",
            frame::MAX_WIDTH,
            frame::MAX_HEIGHT
        );
    }
    let mut geometry: Option<(usize, usize)> = None;

    // The reader thread must never pause: the hardware discards data during any
    // gap between two transfers. Decoding therefore happens here, not there.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    let reader_stop = stop.clone();
    let reader = std::thread::spawn(move || -> Result<(), String> {
        let timeout = Duration::from_millis(200);
        let mut buffer = vec![0_u8; 1024 * 1024];
        while !reader_stop.load(std::sync::atomic::Ordering::Relaxed) {
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

    let mut dropped = 0_u64;
    loop {
        if let Some(limit) = limit
            && started.elapsed() >= limit
        {
            break;
        }
        match receiver.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                assembler.push(&chunk, |event| match event {
                    Event::Frame(frame) => {
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
                            dropped_frames += 1;
                        }
                    }
                    Event::Audio(bytes) => {
                        if audio_path.is_some() && audio_sender.try_send(bytes).is_err() {
                            dropped_audio += 1;
                        }
                    }
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => dropped += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(error) = write_error.lock().unwrap().take() {
            // Consumer gone (for example FFmpeg exited): that is a normal end.
            eprintln!("stopping: {error}");
            break;
        }
        if reported.elapsed() >= Duration::from_secs(5) {
            let stats = assembler.stats;
            let elapsed = started.elapsed().as_secs_f64();
            eprintln!(
                "{} frame(s), {:.1} fps, {} audio block(s), {} bad, {} unknown, {} format change(s), {}/{} dropped by consumer",
                stats.frames,
                stats.frames as f64 / elapsed,
                stats.audio_blocks,
                stats.bad_frames,
                stats.unknown_blocks,
                stats.format_changes,
                dropped_frames,
                dropped_audio
            );
            reported = Instant::now();
        }
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(receiver);
    if let Ok(Err(error)) = reader.join() {
        eprintln!("reader thread: {error}");
    }
    drop(frame_sender);
    drop(audio_sender);
    // The frame writer may sit in a write to a stalled consumer; every frame
    // is flushed as it is written, so there is nothing to wait for.
    drop(frame_writer);
    let _ = audio_writer.join();
    if dropped_frames > 0 || dropped_audio > 0 {
        eprintln!(
            "{dropped_frames} frame(s) and {dropped_audio} audio block(s) dropped by a slow consumer"
        );
    }
    if dropped > 0 {
        eprintln!("{dropped} read timeout(s)");
    }
    let stats = assembler.stats;
    let elapsed = started.elapsed().as_secs_f64();
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

/// Shared state of the isochronous reader: byte counter and a bounded dump.
struct IsoState {
    bytes: u64,
    packets: u64,
    dump: Vec<u8>,
    dump_limit: usize,
}

extern "system" fn iso_callback(transfer: *mut libusb1_sys::libusb_transfer) {
    // SAFETY: the transfer and its user data were created in `observe_iso`
    // and stay alive until every transfer has been freed there.
    unsafe {
        let state = &mut *((*transfer).user_data as *mut IsoState);
        let packets = (*transfer).num_iso_packets as usize;
        let mut offset = 0_usize;
        for i in 0..packets {
            let desc = &*(*transfer).iso_packet_desc.as_ptr().add(i);
            let got = desc.actual_length as usize;
            if got > 0 {
                state.bytes += got as u64;
                state.packets += 1;
                if state.dump.len() < state.dump_limit {
                    let take = got.min(state.dump_limit - state.dump.len());
                    state.dump.extend_from_slice(std::slice::from_raw_parts(
                        (*transfer).buffer.add(offset),
                        take,
                    ));
                }
            }
            offset += desc.length as usize;
        }
        if (*transfer).status == libusb1_sys::constants::LIBUSB_TRANSFER_CANCELLED {
            return;
        }
        libusb1_sys::libusb_submit_transfer(transfer);
    }
}

/// Streams the way the official application does: bank 0x64 register 0x3b
/// := 0x80, register 0x10 := 0x01, alternate setting 2, isochronous reads
/// from endpoint 0x83. Reports the throughput and dumps the first 64 MB to
/// /tmp/hd60s-iso.bin, then stops the stream again (alternate setting 0,
/// register 0x10 := 0x00).
fn observe_iso<T: UsbContext>(context: &T, device: Device<T>, seconds: u64) -> Result<(), String> {
    const PACKET: usize = 32 * 1024; // 1024 x burst 16 x mult 2 per interval
    const PACKETS: usize = 32;
    const TRANSFERS: usize = 8;

    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    // A kernel driver bound to the interface (for example hd60s.ko) is
    // detached first; libusb rebinds nothing, so it stays off until a replug.
    let _ = handle.set_auto_detach_kernel_driver(true);
    handle
        .claim_interface(0)
        .map_err(|error| format!("claiming interface 0: {error}"))?;
    let write_type = rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device);
    let timeout = Duration::from_secs(1);
    let write = |register: u16, data: &[u8]| {
        handle
            .write_control(
                write_type,
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                register,
                data,
                timeout,
            )
            .map(|_| ())
            .map_err(|error| format!("writing bank 0x64 register {register:#04x}: {error}"))
    };
    write(0x3b, &[0x80])?;
    write(0x10, &[0x01])?;
    eprintln!("stage: bank 0x64 register 0x3b := 80, register 0x10 := 01");
    handle
        .set_alternate_setting(0, 2)
        .map_err(|error| format!("selecting alternate setting 2: {error}"))?;
    eprintln!("stage: alternate setting 2 (isochronous) selected, streaming {seconds} s");

    let mut state = Box::new(IsoState {
        bytes: 0,
        packets: 0,
        dump: Vec::new(),
        dump_limit: 64 << 20,
    });
    let mut buffers: Vec<Vec<u8>> = (0..TRANSFERS)
        .map(|_| vec![0_u8; PACKET * PACKETS])
        .collect();
    let mut transfers = Vec::with_capacity(TRANSFERS);
    // SAFETY: plain libusb asynchronous API; every pointer handed to libusb
    // outlives the transfers, which are cancelled and freed below.
    unsafe {
        for buffer in buffers.iter_mut() {
            let transfer = libusb1_sys::libusb_alloc_transfer(PACKETS as i32);
            if transfer.is_null() {
                return Err("allocating an isochronous transfer".into());
            }
            (*transfer).dev_handle = handle.as_raw();
            (*transfer).endpoint = 0x83;
            (*transfer).transfer_type = libusb1_sys::constants::LIBUSB_TRANSFER_TYPE_ISOCHRONOUS;
            (*transfer).timeout = 1000;
            (*transfer).buffer = buffer.as_mut_ptr();
            (*transfer).length = buffer.len() as i32;
            (*transfer).num_iso_packets = PACKETS as i32;
            (*transfer).callback = iso_callback;
            (*transfer).user_data = &mut *state as *mut IsoState as *mut std::ffi::c_void;
            for i in 0..PACKETS {
                (*(*transfer).iso_packet_desc.as_mut_ptr().add(i)).length = PACKET as u32;
            }
            let rc = libusb1_sys::libusb_submit_transfer(transfer);
            if rc != 0 {
                return Err(format!(
                    "submitting an isochronous transfer: libusb error {rc}"
                ));
            }
            transfers.push(transfer);
        }
        let started = Instant::now();
        let mut reported = Instant::now();
        while started.elapsed() < Duration::from_secs(seconds) {
            let tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 100_000,
            };
            libusb1_sys::libusb_handle_events_timeout_completed(
                context.as_raw(),
                &tv,
                std::ptr::null_mut(),
            );
            if reported.elapsed() >= Duration::from_secs(2) {
                eprintln!(
                    "{:.1} MB/s, {} packet(s) with data",
                    state.bytes as f64 / started.elapsed().as_secs_f64() / 1e6,
                    state.packets
                );
                reported = Instant::now();
            }
        }
        for transfer in &transfers {
            libusb1_sys::libusb_cancel_transfer(*transfer);
        }
        for _ in 0..10 {
            let tv = libc::timeval {
                tv_sec: 0,
                tv_usec: 100_000,
            };
            libusb1_sys::libusb_handle_events_timeout_completed(
                context.as_raw(),
                &tv,
                std::ptr::null_mut(),
            );
        }
        for transfer in transfers {
            libusb1_sys::libusb_free_transfer(transfer);
        }
    }
    handle
        .set_alternate_setting(0, 0)
        .map_err(|error| format!("selecting alternate setting 0: {error}"))?;
    write(0x10, &[0x00])?;
    eprintln!("stage: alternate setting 0, register 0x10 := 00");

    let path = "/tmp/hd60s-iso.bin";
    std::fs::write(path, &state.dump).map_err(|error| format!("writing {path}: {error}"))?;
    println!(
        "isochronous: {} byte(s) in {} packet(s), {:.1} MB/s, first {} byte(s) in {path}",
        state.bytes,
        state.packets,
        state.bytes as f64 / seconds as f64 / 1e6,
        state.dump.len()
    );
    Ok(())
}

/// Settings for the `picture` command; `None` leaves a value untouched.
#[derive(Default, Clone)]
struct PictureSettings {
    range: Option<u8>,
    controls: [Option<u8>; 4],
    reset: bool,
}

/// Options for the `edid` command.
#[derive(Default, Clone)]
struct EdidSettings {
    dump: Option<String>,
    write: Option<String>,
    restore: bool,
    fix: bool,
}

fn read_edid<T: UsbContext>(handle: &rusb::DeviceHandle<T>) -> Result<[u8; 256], String> {
    let read_type = rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device);
    let mut edid = [0_u8; 256];
    for offset in (0..256_u16).step_by(16) {
        let chunk = &mut edid[offset as usize..offset as usize + 16];
        let got = handle
            .read_control(
                read_type,
                REGISTER_REQUEST,
                EDID_BANK,
                offset,
                chunk,
                Duration::from_secs(1),
            )
            .map_err(|error| format!("reading EDID bytes {offset}..: {error}"))?;
        if got != 16 {
            return Err(format!("reading EDID bytes {offset}..: got {got} bytes"));
        }
    }
    Ok(edid)
}

fn write_edid<T: UsbContext>(
    handle: &rusb::DeviceHandle<T>,
    edid: &[u8; 256],
) -> Result<(), String> {
    let write_type = rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device);
    for offset in (0..256_u16).step_by(16) {
        let chunk = &edid[offset as usize..offset as usize + 16];
        handle
            .write_control(
                write_type,
                REGISTER_REQUEST,
                EDID_BANK,
                offset,
                chunk,
                Duration::from_secs(1),
            )
            .map_err(|error| format!("writing EDID bytes {offset}..: {error}"))?;
    }
    Ok(())
}

/// Reads, saves, writes or restores the EDID the device presents to its HDMI
/// source (bank 0xa0), the way the official application does: 16-byte pieces,
/// then a read-back. A block that fails validation is refused unless `--fix`
/// recomputes its checksums. The source only re-reads the EDID on a hot-plug,
/// so a change takes effect when the HDMI cable is reconnected.
fn edid_command<T: UsbContext>(device: Device<T>, settings: EdidSettings) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    let current = read_edid(&handle)?;
    println!("device: {}", edid::summary(&current));
    if let Err(error) = edid::validate(&current) {
        println!("device EDID is invalid: {error}");
    }
    if let Some(path) = &settings.dump {
        std::fs::write(path, current).map_err(|error| format!("writing {path}: {error}"))?;
        println!("saved 256 bytes to {path}");
    }

    let wanted: Option<[u8; 256]> = if settings.restore {
        Some(edid::FACTORY)
    } else if let Some(path) = &settings.write {
        let bytes = std::fs::read(path).map_err(|error| format!("reading {path}: {error}"))?;
        let mut block: [u8; 256] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| format!("{path}: expected 256 bytes, got {}", bytes.len()))?;
        if settings.fix {
            edid::fix_checksums(&mut block);
        }
        edid::validate(&block)
            .map_err(|error| format!("{path}: {error} (use --fix to recompute)"))?;
        Some(block)
    } else {
        None
    };

    if let Some(block) = wanted {
        if block == current {
            println!("device already holds this EDID; nothing written");
            return Ok(());
        }
        write_edid(&handle, &block)?;
        let back = read_edid(&handle)?;
        let differing = back
            .iter()
            .zip(block.iter())
            .filter(|(a, b)| a != b)
            .count();
        if differing == 0 {
            println!("written and verified: {}", edid::summary(&back));
        } else {
            println!(
                "written; read-back differs in {differing} byte(s) (the device may adjust fields): {}",
                edid::summary(&back)
            );
        }
        println!("reconnect the HDMI cable so the source reads the new EDID");
    }
    Ok(())
}

/// The three microcontroller status queries the official driver makes at
/// plug-in, through the shared `Control` (which claims the interface and
/// runs the plug-in sequence first).
fn mcu_status() -> Result<(), String> {
    let control = Control::open()?;
    for (command, meaning) in hd60s_linux::control::MCU_STATUS {
        let reply = control.mcu(command)?;
        println!(
            "command {command:#04x} ({meaning}): reply {:02x} {:02x} {:02x}",
            reply[0], reply[1], reply[2]
        );
    }
    println!("firmware build date: {}", control.firmware_date()?);
    Ok(())
}

fn audio<T: UsbContext>(device: Device<T>, gain: Option<u8>) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    let timeout = Duration::from_secs(1);
    if let Some(gain) = gain {
        handle
            .write_control(
                rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device),
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                AUDIO_GAIN_REGISTER,
                &[gain],
                timeout,
            )
            .map_err(|error| format!("writing audio gain: {error}"))?;
    }
    let mut value = [0_u8; 1];
    handle
        .read_control(
            rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device),
            REGISTER_REQUEST,
            SIGNAL_REGISTER_BANK,
            AUDIO_GAIN_REGISTER,
            &mut value,
            timeout,
        )
        .map_err(|error| format!("reading audio gain: {error}"))?;
    let gain = value[0];
    if gain == 0 {
        println!("audio gain: 0 (muted)");
    } else {
        println!(
            "audio gain: {gain} ({:+.1} dB relative to 0x80)",
            (gain as f64 - 128.0) * 0.5
        );
    }
    Ok(())
}

/// Shows or changes the HDMI colour range and the picture controls, using
/// exactly the register writes the official application makes:
/// bank 0x64 register 0x12 for the range, register 0x13 (four bytes) for
/// brightness, contrast, saturation and hue.
fn picture<T: UsbContext>(device: Device<T>, settings: PictureSettings) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    let read_type = rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device);
    let write_type = rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device);
    let timeout = Duration::from_secs(1);
    let mut registers = [0_u8; 32];
    let read_all = |registers: &mut [u8; 32]| -> Result<(), String> {
        handle
            .read_control(
                read_type,
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                0,
                registers,
                timeout,
            )
            .map(|_| ())
            .map_err(|error| format!("reading bank 0x64 registers: {error}"))
    };
    read_all(&mut registers)?;

    let mut controls = [
        registers[PICTURE_REGISTER as usize],
        registers[PICTURE_REGISTER as usize + 1],
        registers[PICTURE_REGISTER as usize + 2],
        registers[PICTURE_REGISTER as usize + 3],
    ];
    let mut changed = false;
    if settings.reset {
        controls = [0x80; 4];
        changed = true;
    }
    for (slot, value) in controls.iter_mut().zip(settings.controls) {
        if let Some(value) = value {
            *slot = value;
            changed = true;
        }
    }
    if changed {
        handle
            .write_control(
                write_type,
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                PICTURE_REGISTER,
                &controls,
                timeout,
            )
            .map_err(|error| format!("writing picture controls: {error}"))?;
    }
    if let Some(range) = settings.range {
        handle
            .write_control(
                write_type,
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                COLOUR_RANGE_REGISTER,
                &[range],
                timeout,
            )
            .map_err(|error| format!("writing colour range: {error}"))?;
    }

    read_all(&mut registers)?;
    let range = registers[COLOUR_RANGE_REGISTER as usize];
    println!(
        "colour range: {} ({range:#04x})",
        match range {
            0 => "bypass",
            1 => "shrink (the application calls this expanded)",
            2 => "expand",
            _ => "unset (power-on default)",
        }
    );
    for (name, value) in ["brightness", "contrast", "saturation", "hue"]
        .iter()
        .zip(&registers[PICTURE_REGISTER as usize..PICTURE_REGISTER as usize + 4])
    {
        println!(
            "{name:<11} {value:>3}{}",
            if *value == 0x80 { "  (neutral)" } else { "" }
        );
    }
    Ok(())
}

/// Reads bank 0x64 registers 0x00..0x1f the way the official driver polls them
/// every 105 ms, and prints the detected input timing. With `seconds`, keeps
/// watching and prints only changes.
fn read_signal<T: UsbContext>(device: Device<T>, seconds: Option<u64>) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    // Vendor/device form as seen on the wire; no interface claim is needed.
    let request_type = rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device);
    let mut registers = [0_u8; 32];
    let mut previous: Option<[u8; 32]> = None;
    let started = Instant::now();
    loop {
        let length = handle
            .read_control(
                request_type,
                REGISTER_REQUEST,
                SIGNAL_REGISTER_BANK,
                0,
                &mut registers,
                Duration::from_secs(1),
            )
            .map_err(|error| format!("reading bank 0x64 registers: {error}"))?;
        if length != registers.len() {
            return Err(format!("expected 32 bytes, got {length}"));
        }
        if previous != Some(registers) {
            let word = |at: usize| u16::from_le_bytes([registers[at], registers[at + 1]]);
            let (total_lines, total_pixels) = (word(4), word(6));
            let (height, width) = (word(8), word(10));
            let raw = registers
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            if width == 0 && height == 0 {
                println!("no signal  [{raw}]");
            } else {
                println!(
                    "{width}x{height} active, {total_pixels}x{total_lines} total, {} Hz  [{raw}]",
                    registers[12]
                );
            }
            previous = Some(registers);
        }
        match seconds {
            Some(limit) if started.elapsed() < Duration::from_secs(limit) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => break,
        }
    }
    Ok(())
}

fn read_direct_startup_status<T: UsbContext>(device: Device<T>) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    // A kernel driver bound to the interface (for example hd60s.ko) is
    // detached first; libusb rebinds nothing, so it stays off until a replug.
    let _ = handle.set_auto_detach_kernel_driver(true);
    handle
        .claim_interface(0)
        .map_err(|error| format!("claiming interface 0: {error}"))?;

    let request_type = rusb::request_type(Direction::In, RequestType::Class, Recipient::Interface);
    let mut response = [0_u8; 1];
    let length = handle
        .read_control(
            request_type,
            REGISTER_REQUEST,
            HDMI_REGISTER_BANK,
            STARTUP_STATUS_REGISTER,
            &mut response,
            Duration::from_secs(1),
        )
        .map_err(|error| format!("reading direct-form startup status register: {error}"))?;
    if length != response.len() {
        return Err(format!(
            "startup status returned {length} byte(s), expected 1"
        ));
    }

    println!(
        "direct class-interface IN request=0x{REGISTER_REQUEST:02x} value=0x{HDMI_REGISTER_BANK:04x} index=0x{STARTUP_STATUS_REGISTER:04x}: 0x{:02x}",
        response[0]
    );
    Ok(())
}

enum Operation {
    Inspect,
    ObserveInterrupt(u64),
    ObserveStream(u64),
    ObserveIso(u64),
    Capture(Option<u64>, Option<String>, bool),
    Signal(Option<u64>),
    Picture(PictureSettings),
    Audio(Option<u8>),
    Edid(EdidSettings),
    Mcu,
    Status,
}

/// Holds the streaming interface while registers are used from this
/// process, so that no other process streams meanwhile: control transfers
/// next to a stream owned by a different handle have hung the card's
/// microcontroller until the next power cycle.
fn exclusive<T: UsbContext>(device: &Device<T>) -> Result<rusb::DeviceHandle<T>, String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    let _ = handle.set_auto_detach_kernel_driver(true);
    match handle.claim_interface(0) {
        Ok(()) => {
            hd60s_linux::control::initialise_handle(&handle)?;
            Ok(handle)
        }
        Err(rusb::Error::Busy) => Err(
            "the HD60 S is streaming in another process (hd60s-linux serve?); \
             talking to its registers from here could hang the card — use the control panel at \
             http://127.0.0.1:8060/ or stop the service first"
                .into(),
        ),
        Err(error) => Err(format!("claiming interface 0: {error}")),
    }
}

fn run(show_serial: bool, operation: Operation) -> Result<(), String> {
    let context = Context::new().map_err(|error| format!("initializing libusb: {error}"))?;
    let devices = context
        .devices()
        .map_err(|error| format!("enumerating USB devices: {error}"))?;

    for device in devices.iter() {
        let descriptor = device
            .device_descriptor()
            .map_err(|error| format!("reading a USB device descriptor: {error}"))?;
        if hd60s_revision(descriptor.vendor_id(), descriptor.product_id()).is_some() {
            match operation {
                Operation::ObserveInterrupt(seconds) => {
                    return observe_interrupt(device, seconds)
                        .map_err(|error| format!("observing HD60 S interrupt endpoint: {error}"));
                }
                Operation::ObserveStream(seconds) => return observe_stream(device, seconds),
                Operation::ObserveIso(seconds) => return observe_iso(&context, device, seconds),
                Operation::Capture(seconds, ref audio, native) => {
                    return capture(device, seconds, audio.as_deref(), native);
                }
                Operation::Signal(seconds) => {
                    let _claim = exclusive(&device)?;
                    return read_signal(device, seconds);
                }
                Operation::Picture(ref settings) => {
                    let _claim = exclusive(&device)?;
                    return picture(device, settings.clone());
                }
                Operation::Audio(gain) => {
                    let _claim = exclusive(&device)?;
                    return audio(device, gain);
                }
                Operation::Edid(ref settings) => {
                    let _claim = exclusive(&device)?;
                    return edid_command(device, settings.clone());
                }
                Operation::Mcu => return mcu_status(),
                Operation::Status => return read_direct_startup_status(device),
                Operation::Inspect => {}
            }
            return inspect_device(device, descriptor, show_serial)
                .map_err(|error| format!("inspecting HD60 S: {error}"));
        }
    }

    Err(format!("no HD60 S found; looked for {}", known_ids()))
}

/// Parses `picture` options; values are 0..=255 with 128 as neutral.
fn parse_picture(arguments: &[String]) -> Result<PictureSettings, std::num::ParseIntError> {
    let mut settings = PictureSettings::default();
    let mut iter = arguments.iter();
    while let Some(argument) = iter.next() {
        let value = |iter: &mut std::slice::Iter<String>| -> Result<u8, std::num::ParseIntError> {
            iter.next().map(String::as_str).unwrap_or("").parse::<u8>()
        };
        match argument.as_str() {
            "--reset" => settings.reset = true,
            "--range" => {
                settings.range = Some(match iter.next().map(String::as_str) {
                    Some("bypass" | "standard") => 0,
                    Some("shrink" | "expanded") => 1,
                    Some("expand") => 2,
                    other => other.unwrap_or("").parse::<u8>()?,
                })
            }
            "--brightness" => settings.controls[0] = Some(value(&mut iter)?),
            "--contrast" => settings.controls[1] = Some(value(&mut iter)?),
            "--saturation" => settings.controls[2] = Some(value(&mut iter)?),
            "--hue" => settings.controls[3] = Some(value(&mut iter)?),
            _ => "x".parse::<u8>().map(|_| ())?,
        }
    }
    Ok(settings)
}

fn main() -> ExitCode {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let show_serial = arguments.iter().any(|argument| argument == "--show-serial");
    let seconds = || {
        arguments
            .get(1)
            .map(String::as_str)
            .unwrap_or("10")
            .parse::<u64>()
    };
    let operation = match arguments.first().map(String::as_str) {
        Some("observe") => seconds().map(Operation::ObserveInterrupt),
        Some("observe-stream") => seconds().map(Operation::ObserveStream),
        Some("observe-iso") => seconds().map(Operation::ObserveIso),
        Some("capture") => {
            let audio = arguments
                .iter()
                .position(|argument| argument == "--audio")
                .and_then(|at| arguments.get(at + 1))
                .cloned();
            let native = arguments.iter().any(|argument| argument == "--native");
            let limit = arguments
                .get(1)
                .filter(|argument| !argument.starts_with("--"))
                .map(|argument| argument.parse::<u64>());
            match limit {
                Some(Ok(value)) => Ok(Operation::Capture(Some(value), audio, native)),
                Some(Err(error)) => Err(error),
                None => Ok(Operation::Capture(None, audio, native)),
            }
        }
        Some("signal") => match arguments.get(1) {
            Some(value) => value
                .parse::<u64>()
                .map(|seconds| Operation::Signal(Some(seconds))),
            None => Ok(Operation::Signal(None)),
        },
        Some("picture") => parse_picture(&arguments[1..]).map(Operation::Picture),
        Some("audio") => match arguments.get(1).map(String::as_str) {
            Some("--mute") => Ok(Operation::Audio(Some(0))),
            Some("--gain") => arguments
                .get(2)
                .map(String::as_str)
                .unwrap_or("")
                .parse::<u8>()
                .map(|gain| Operation::Audio(Some(gain))),
            _ => Ok(Operation::Audio(None)),
        },
        Some("edid") => {
            let mut settings = EdidSettings::default();
            let mut iter = arguments[1..].iter();
            while let Some(argument) = iter.next() {
                match argument.as_str() {
                    "--dump" => settings.dump = iter.next().cloned(),
                    "--write" => settings.write = iter.next().cloned(),
                    "--restore" => settings.restore = true,
                    "--fix" => settings.fix = true,
                    _ => {}
                }
            }
            Ok(Operation::Edid(settings))
        }
        Some("mcu") => Ok(Operation::Mcu),
        Some("status") => Ok(Operation::Status),
        _ => Ok(Operation::Inspect),
    };
    if arguments.first().map(String::as_str) == Some("report") {
        let capture = if arguments.iter().any(|a| a == "--no-capture") {
            0
        } else {
            3
        };
        print!("{}", hd60s_linux::report::text(show_serial, capture));
        return ExitCode::SUCCESS;
    }
    if arguments.first().map(String::as_str) == Some("serve") {
        let option = |flag: &str| {
            arguments
                .iter()
                .position(|argument| argument == flag)
                .and_then(|at| arguments.get(at + 1))
                .cloned()
        };
        let name = option("--name").unwrap_or_else(|| "Elgato HD60 S".to_string());
        let source = match option("--from-file") {
            Some(path) => Source::File {
                path,
                fps: option("--fps")
                    .and_then(|value| value.parse::<f64>().ok())
                    .unwrap_or(60.0),
            },
            None => Source::Usb,
        };
        let panel = match option("--panel").as_deref() {
            Some("off") | Some("none") => None,
            Some(address) => Some(address.to_string()),
            None => Some("127.0.0.1:8060".to_string()),
        };
        let tray = !matches!(option("--tray").as_deref(), Some("off") | Some("none"));
        let record_dir = option("--record-dir")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::process::Command::new("xdg-user-dir")
                    .arg("VIDEOS")
                    .output()
                    .ok()
                    .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Videos"))
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
            });
        let record_encoder = match option("--record-encoder") {
            Some(name) => match hd60s_linux::record::Encoder::parse(&name) {
                Some(encoder) => encoder,
                None => {
                    eprintln!("error: --record-encoder must be x264 or vaapi");
                    return ExitCode::FAILURE;
                }
            },
            None => hd60s_linux::record::Encoder::X264,
        };
        return match serve(
            &name,
            source,
            hd60s_linux::serve::Options {
                panel,
                tray,
                record_dir,
                record_encoder,
            },
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let operation = match operation {
        Ok(operation) => operation,
        Err(_) => {
            eprintln!(
                "error: usage: hd60s-linux [status|signal|picture|audio|edid|mcu|report|serve|observe|observe-stream|observe-iso|capture] \\
                 [SECONDS] [--audio FILE] [--native]\n       picture [--range bypass|shrink|expand] \\
                 [--brightness N] [--contrast N] [--saturation N] [--hue N] [--reset]\n       audio [--gain N|--mute]\n       edid [--dump FILE] [--write FILE [--fix]] [--restore]\n       serve [--name NAME] [--panel ADDR|off] [--tray off] [--record-dir DIR] [--record-encoder x264|vaapi] [--from-file RAW [--fps N]]\n       report [--show-serial] [--no-capture]"
            );
            return ExitCode::FAILURE;
        }
    };
    match run(show_serial, operation) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usb_names_are_stable_for_protocol_logs() {
        assert_eq!(transfer_name(TransferType::Bulk), "bulk");
        assert_eq!(transfer_name(TransferType::Isochronous), "isochronous");
        assert_eq!(direction_name(Direction::In), "in");
        assert_eq!(direction_name(Direction::Out), "out");
    }

    #[test]
    fn direct_startup_status_is_a_class_interface_read() {
        assert_eq!(
            rusb::request_type(Direction::In, RequestType::Class, Recipient::Interface),
            0xa1
        );
        assert_eq!(REGISTER_REQUEST, 0xc0);
        assert_eq!(HDMI_REGISTER_BANK, 0x0098);
        assert_eq!(STARTUP_STATUS_REGISTER, 0x003b);
    }
}
