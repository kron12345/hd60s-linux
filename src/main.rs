use std::process::ExitCode;
use std::time::Duration;

use rusb::{Context, Device, DeviceDescriptor, Direction, TransferType, UsbContext};

const ELGATO_VENDOR_ID: u16 = 0x0fd9;
const HD60S_PRODUCT_ID: u16 = 0x005e;

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

fn run(show_serial: bool) -> Result<(), String> {
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
    let show_serial = std::env::args()
        .skip(1)
        .any(|argument| argument == "--show-serial");
    match run(show_serial) {
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
}
