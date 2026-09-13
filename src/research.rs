//! Research tools kept for protocol work: descriptor inspection, watching
//! the interrupt endpoint, raw bulk and isochronous dumps, and the
//! class/interface status read from the static driver analysis. Nothing
//! here is needed to use the card; see docs/protocol.md for what each one
//! established.

use std::fs::File;
use std::io::Write;
use std::time::{Duration, Instant};

use rusb::{Device, DeviceDescriptor, Direction, Recipient, RequestType, TransferType, UsbContext};

use crate::device::revision as hd60s_revision;

const REGISTER_REQUEST: u8 = 0xc0;
const HDMI_REGISTER_BANK: u16 = 0x0098;
const STARTUP_STATUS_REGISTER: u16 = 0x003b;
/// Rev. 4 register bank the official driver polls for the input timing.
const SIGNAL_REGISTER_BANK: u16 = 0x0064;

pub fn transfer_name(transfer_type: TransferType) -> &'static str {
    match transfer_type {
        TransferType::Control => "control",
        TransferType::Isochronous => "isochronous",
        TransferType::Bulk => "bulk",
        TransferType::Interrupt => "interrupt",
    }
}

pub fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::In => "in",
        Direction::Out => "out",
    }
}

pub fn inspect_device<T: UsbContext>(
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

pub fn observe_interrupt<T: UsbContext>(device: Device<T>, seconds: u64) -> rusb::Result<()> {
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

pub fn observe_stream<T: UsbContext>(device: Device<T>, seconds: u64) -> Result<(), String> {
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
pub fn observe_iso<T: UsbContext>(
    context: &T,
    device: Device<T>,
    seconds: u64,
) -> Result<(), String> {
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

/// The direct-form class/interface read from the static driver analysis
/// (bank 0x98 register 0x3b); Rev. 4 answers vendor/device requests
/// instead, see `Control`.
pub fn read_direct_startup_status<T: UsbContext>(device: Device<T>) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusb::{Direction, Recipient, RequestType, TransferType};

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
