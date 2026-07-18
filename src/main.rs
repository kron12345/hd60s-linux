use std::process::ExitCode;
use std::time::{Duration, Instant};
use std::{fs::File, io::Write};

use rusb::{
    Context, Device, DeviceDescriptor, Direction, Recipient, RequestType, TransferType, UsbContext,
};

const ELGATO_VENDOR_ID: u16 = 0x0fd9;
const HD60S_PRODUCT_ID: u16 = 0x005e;
const REGISTER_REQUEST: u8 = 0xc0;
const HDMI_REGISTER_BANK: u16 = 0x0098;
const STARTUP_STATUS_REGISTER: u16 = 0x003b;

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
        "HD60 S {:04x}:{:04x} at bus {:03} address {:03}",
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
        if descriptor.vendor_id() == ELGATO_VENDOR_ID && descriptor.product_id() == HD60S_PRODUCT_ID
        {
            match operation {
                Operation::ObserveInterrupt(seconds) => {
                    return observe_interrupt(device, seconds)
                        .map_err(|error| format!("observing HD60 S interrupt endpoint: {error}"));
                }
                Operation::ObserveStream(seconds) => return observe_stream(device, seconds),
                Operation::Status => return read_direct_startup_status(device),
                Operation::Inspect => {}
            }
            return inspect_device(device, descriptor, show_serial)
                .map_err(|error| format!("inspecting HD60 S: {error}"));
        }
    }

    Err(format!(
        "HD60 S {:04x}:{:04x} not found",
        ELGATO_VENDOR_ID, HD60S_PRODUCT_ID
    ))
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
        Some("status") => Ok(Operation::Status),
        _ => Ok(Operation::Inspect),
    };
    let operation = match operation {
        Ok(operation) => operation,
        Err(_) => {
            eprintln!("error: usage: hd60s-linux [status|observe|observe-stream] [SECONDS]");
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
