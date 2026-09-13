//! Finding the device: the product IDs the official driver's INF maps onto
//! one driver, and a lookup over the USB bus.

use rusb::{Device, UsbContext};

pub const ELGATO_VENDOR_ID: u16 = 0x0fd9;

/// Product IDs from the official Windows driver's INF, with its names.
pub const PRODUCT_IDS: [(u16, &str); 4] = [
    (0x004f, "HD60 S"),
    (0x005e, "HD60 S Rev. 2"),
    (0x0074, "HD60 S Rev. 3"),
    (0x0076, "HD60 S Rev. 4"),
];

/// Returns the INF revision name when the IDs name a supported device.
pub fn revision(vendor_id: u16, product_id: u16) -> Option<&'static str> {
    if vendor_id != ELGATO_VENDOR_ID {
        return None;
    }
    PRODUCT_IDS
        .iter()
        .find(|(id, _)| *id == product_id)
        .map(|(_, name)| *name)
}

/// The first HD60 S on the bus, if any.
pub fn find<T: UsbContext>(context: &T) -> Option<Device<T>> {
    context.devices().ok()?.iter().find(|device| {
        device
            .device_descriptor()
            .map(|d| revision(d.vendor_id(), d.product_id()).is_some())
            .unwrap_or(false)
    })
}

/// Every known ID as "vendor:product (name)", for error messages.
pub fn known_ids() -> String {
    PRODUCT_IDS
        .iter()
        .map(|(id, name)| format!("{ELGATO_VENDOR_ID:04x}:{id:04x} ({name})"))
        .collect::<Vec<_>>()
        .join(", ")
}
