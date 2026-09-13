use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::{fs::File, io::Write};

use hd60s_linux::frame::{self, Assembler, Event};
use rusb::{
    Context, Device, DeviceDescriptor, Direction, Recipient, RequestType, TransferType, UsbContext,
};

const ELGATO_VENDOR_ID: u16 = 0x0fd9;
/// Product IDs from the official Windows driver's INF, which maps all four
/// hardware revisions onto the same driver.
const HD60S_PRODUCT_IDS: [(u16, &str); 4] = [
    (0x004f, "HD60 S"),
    (0x005e, "HD60 S Rev. 2"),
    (0x0074, "HD60 S Rev. 3"),
    (0x0076, "HD60 S Rev. 4"),
];

/// Returns the INF revision name when the descriptor names a supported device.
fn hd60s_revision(vendor_id: u16, product_id: u16) -> Option<&'static str> {
    if vendor_id != ELGATO_VENDOR_ID {
        return None;
    }
    HD60S_PRODUCT_IDS
        .iter()
        .find(|(id, _)| *id == product_id)
        .map(|(_, name)| *name)
}
const REGISTER_REQUEST: u8 = 0xc0;
const HDMI_REGISTER_BANK: u16 = 0x0098;
const STARTUP_STATUS_REGISTER: u16 = 0x003b;
/// Rev. 4 register bank the official driver polls for the input timing.
const SIGNAL_REGISTER_BANK: u16 = 0x0064;

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
/// The output is YUYV 4:2:2, 1920x1080, 60 Hz, so it can be consumed directly:
/// `hd60s-linux capture | ffmpeg -f rawvideo -pix_fmt yuyv422 -s 1920x1080 -r 60 -i - ...`
/// With `--audio FILE` the embedded audio bytes are written there as well
/// (16-bit little-endian, stereo, 48 kHz).
fn capture<T: UsbContext + 'static>(
    device: Device<T>,
    seconds: Option<u64>,
    audio_path: Option<&str>,
) -> Result<(), String> {
    let handle = device
        .open()
        .map_err(|error| format!("opening device: {error}"))?;
    handle
        .claim_interface(0)
        .map_err(|error| format!("claiming interface 0: {error}"))?;
    handle
        .set_alternate_setting(0, 4)
        .map_err(|error| format!("selecting interface 0 alternate setting 4: {error}"))?;

    let mut audio_file = match audio_path {
        Some(path) => {
            Some(File::create(path).map_err(|error| format!("creating {path}: {error}"))?)
        }
        None => None,
    };

    let mut stdout = std::io::BufWriter::with_capacity(frame::FRAME_BYTES, std::io::stdout());
    let mut assembler = Assembler::new();
    let started = Instant::now();
    let limit = seconds.map(Duration::from_secs);
    let mut reported = Instant::now();
    let mut write_error: Option<String> = None;

    eprintln!(
        "capturing {}x{} yuyv422 from bulk endpoint 0x83",
        frame::WIDTH,
        frame::HEIGHT
    );

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
                    Event::Frame(pixels) => {
                        if write_error.is_none()
                            && let Err(error) = stdout.write_all(&pixels)
                        {
                            write_error = Some(format!("writing frame to stdout: {error}"));
                        }
                    }
                    Event::Audio(bytes) => {
                        if let Some(file) = audio_file.as_mut()
                            && write_error.is_none()
                            && let Err(error) = file.write_all(&bytes)
                        {
                            write_error = Some(format!("writing audio: {error}"));
                        }
                    }
                });
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => dropped += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Some(error) = write_error {
            // Consumer gone (for example FFmpeg exited): that is a normal end.
            eprintln!("stopping: {error}");
            break;
        }
        if reported.elapsed() >= Duration::from_secs(5) {
            let stats = assembler.stats;
            let elapsed = started.elapsed().as_secs_f64();
            eprintln!(
                "{} frame(s), {:.1} fps, {} audio block(s), {} short, {} unknown",
                stats.frames,
                stats.frames as f64 / elapsed,
                stats.audio_blocks,
                stats.short_frames,
                stats.unknown_blocks
            );
            reported = Instant::now();
        }
    }

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    drop(receiver);
    if let Ok(Err(error)) = reader.join() {
        eprintln!("reader thread: {error}");
    }
    let _ = stdout.flush();
    if dropped > 0 {
        eprintln!("{dropped} read timeout(s)");
    }
    let stats = assembler.stats;
    let elapsed = started.elapsed().as_secs_f64();
    eprintln!(
        "capture ended: {} frame(s) in {:.1} s ({:.2} fps), {} audio block(s), {} short frame(s)",
        stats.frames,
        elapsed,
        stats.frames as f64 / elapsed,
        stats.audio_blocks,
        stats.short_frames
    );
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
    Capture(Option<u64>, Option<String>),
    Signal(Option<u64>),
    Status,
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
                Operation::Capture(seconds, ref audio) => {
                    return capture(device, seconds, audio.as_deref());
                }
                Operation::Signal(seconds) => return read_signal(device, seconds),
                Operation::Status => return read_direct_startup_status(device),
                Operation::Inspect => {}
            }
            return inspect_device(device, descriptor, show_serial)
                .map_err(|error| format!("inspecting HD60 S: {error}"));
        }
    }

    let known = HD60S_PRODUCT_IDS
        .iter()
        .map(|(id, name)| format!("{ELGATO_VENDOR_ID:04x}:{id:04x} ({name})"))
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!("no HD60 S found; looked for {known}"))
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
        Some("capture") => {
            let audio = arguments
                .iter()
                .position(|argument| argument == "--audio")
                .and_then(|at| arguments.get(at + 1))
                .cloned();
            let limit = arguments
                .get(1)
                .filter(|argument| !argument.starts_with("--"))
                .map(|argument| argument.parse::<u64>());
            match limit {
                Some(Ok(value)) => Ok(Operation::Capture(Some(value), audio)),
                Some(Err(error)) => Err(error),
                None => Ok(Operation::Capture(None, audio)),
            }
        }
        Some("signal") => match arguments.get(1) {
            Some(value) => value
                .parse::<u64>()
                .map(|seconds| Operation::Signal(Some(seconds))),
            None => Ok(Operation::Signal(None)),
        },
        Some("status") => Ok(Operation::Status),
        _ => Ok(Operation::Inspect),
    };
    let operation = match operation {
        Ok(operation) => operation,
        Err(_) => {
            eprintln!(
                "error: usage: hd60s-linux [status|signal|observe|observe-stream|capture] \
                 [SECONDS] [--audio FILE]"
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
