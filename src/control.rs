//! Everything the device can be asked or told over its control endpoint:
//! the timing registers, picture controls, colour range, audio gain, the
//! EDID and the microcontroller status commands. None of it needs the
//! streaming interface, so it works next to a running capture.
//!
//! All requests are vendor/device control transfers (`bmRequestType`
//! 0x40/0xc0, `bRequest` 0xc0) with the bank in `wValue` and the register in
//! `wIndex`, exactly as the official driver issues them.

use std::sync::Arc;
use std::time::{Duration, Instant};

use rusb::{Context, Device, DeviceHandle, Direction, Recipient, RequestType};

use crate::device;
use crate::edid;

const REQUEST: u8 = 0xc0;
const BANK: u16 = 0x0064;
const EDID_BANK: u16 = 0x00a0;
const MCU_PROXY: u16 = 0x5066;
pub const REG_STREAM: u16 = 0x10;
pub const REG_COLOUR_RANGE: u16 = 0x12;
pub const REG_PICTURE: u16 = 0x13;
pub const REG_AUDIO_GAIN: u16 = 0x3b;
/// Payload of request 0xec in the official driver's plug-in sequence;
/// meaning unknown, sent unchanged.
const INIT_PAYLOAD: [u8; 10] = [0xb8, 0x22, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x50, 0xca];

/// Microcontroller commands that must never be sent: 0x60 turns the light
/// strip on, unlocks the system registers, sets the boot-select bit and
/// resets the MCU into its bootloader.
pub const MCU_FORBIDDEN: [u8; 1] = [0x60];
/// The status commands the official driver issues at PnP time.
pub const MCU_STATUS: [(u8, &str); 3] = [
    (0x57, "status"),
    (0x58, "firmware build date"),
    (0x59, "presence handshake"),
];

/// Detected input timing, decoded from bank 0x64 registers 4..12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    pub width: u16,
    pub height: u16,
    pub total_width: u16,
    pub total_height: u16,
    pub refresh: u8,
}

impl Timing {
    pub fn present(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

impl std::fmt::Display for Timing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.present() {
            write!(
                f,
                "{}x{} active, {}x{} total, {} Hz",
                self.width, self.height, self.total_width, self.total_height, self.refresh
            )
        } else {
            write!(f, "no signal")
        }
    }
}

/// Snapshot of the adjustable registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub colour_range: u8,
    /// brightness, contrast, saturation, hue; 0x80 is neutral
    pub picture: [u8; 4],
    pub audio_gain: u8,
}

pub fn colour_range_name(value: u8) -> &'static str {
    match value {
        0 => "bypass",
        1 => "shrink",
        2 => "expand",
        _ => "unset",
    }
}

/// Audio gain in dB relative to the neutral 0x80; about 0.5 dB per step.
pub fn gain_db(value: u8) -> f64 {
    (value as f64 - 128.0) * 0.5
}

/// An open control connection to the device.
pub struct Control {
    handle: Arc<DeviceHandle<Context>>,
    pub revision: &'static str,
    pub product_id: u16,
    pub device_version: String,
    pub speed: rusb::Speed,
    pub bus: u8,
    pub address: u8,
    pub serial: Option<String>,
}

impl Control {
    /// Opens the first HD60 S on the bus for exclusive use: the streaming
    /// interface is claimed for as long as the `Control` lives, so no other
    /// process can stream meanwhile. Control transfers next to a stream run
    /// by a *different* handle have hung the card's microcontroller until
    /// the next power cycle; a process that streams itself talks to the
    /// registers through its own handle (`from_handle`), as the official
    /// driver does.
    pub fn open() -> Result<Self, String> {
        let context = Context::new().map_err(|error| format!("initializing libusb: {error}"))?;
        let device = device::find(&context)
            .ok_or_else(|| format!("no HD60 S found; looked for {}", device::known_ids()))?;
        let handle = device
            .open()
            .map_err(|error| format!("opening device: {error}"))?;
        let _ = handle.set_auto_detach_kernel_driver(true);
        match handle.claim_interface(0) {
            Ok(()) => {}
            Err(rusb::Error::Busy) => {
                return Err(
                    "the HD60 S is streaming in another process (hd60s-linux serve?); \
                     talking to its registers from here could hang the card — use the control \
                     panel at http://127.0.0.1:8060/ or stop the service first"
                        .into(),
                );
            }
            Err(error) => return Err(format!("claiming interface 0: {error}")),
        }
        let control = Self::from_handle(Arc::new(handle), &device)?;
        control.initialise()?;
        Ok(control)
    }

    /// Wraps a handle that the caller already streams with.
    pub fn from_handle(
        handle: Arc<DeviceHandle<Context>>,
        device: &Device<Context>,
    ) -> Result<Self, String> {
        let descriptor = device
            .device_descriptor()
            .map_err(|error| format!("reading the device descriptor: {error}"))?;
        let serial = handle
            .read_languages(Duration::from_secs(1))
            .ok()
            .and_then(|languages| languages.first().copied())
            .and_then(|language| {
                handle
                    .read_serial_number_string(language, &descriptor, Duration::from_secs(1))
                    .ok()
            });
        Ok(Self {
            handle,
            revision: device::revision(descriptor.vendor_id(), descriptor.product_id())
                .unwrap_or("HD60 S"),
            product_id: descriptor.product_id(),
            device_version: descriptor.device_version().to_string(),
            speed: device.speed(),
            bus: device.bus_number(),
            address: device.address(),
            serial,
        })
    }

    /// The plug-in sequence the official driver sends before anything else,
    /// replayed verbatim: request 0xec with its fixed payload, 0xc1 with
    /// 0xc039 and 0x4134, then 0xc1/0x0039 read back as 01. From a true
    /// power-on state the microcontroller proxy does not work without it —
    /// a bare proxy command then hangs the device's USB controller until the
    /// next power cycle (measured 2026-09-13). The 0xc039 write may stall
    /// when the device is already initialised; that is not an error.
    pub fn initialise(&self) -> Result<(), String> {
        initialise_handle(&self.handle)
    }

    /// Whether a kernel driver currently owns the streaming interface.
    pub fn kernel_driver_active(&self) -> Option<bool> {
        self.handle.kernel_driver_active(0).ok()
    }

    fn read(&self, bank: u16, register: u16, buffer: &mut [u8]) -> Result<usize, String> {
        let request_type =
            rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device);
        self.handle
            .read_control(
                request_type,
                REQUEST,
                bank,
                register,
                buffer,
                Duration::from_secs(1),
            )
            .map_err(|error| format!("reading bank {bank:#06x} register {register:#04x}: {error}"))
    }

    fn write(&self, bank: u16, register: u16, data: &[u8]) -> Result<(), String> {
        let request_type =
            rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device);
        self.handle
            .write_control(
                request_type,
                REQUEST,
                bank,
                register,
                data,
                Duration::from_secs(1),
            )
            .map(|_| ())
            .map_err(|error| format!("writing bank {bank:#06x} register {register:#04x}: {error}"))
    }

    /// Bank 0x64 registers 0x00..0x3f.
    pub fn registers(&self) -> Result<[u8; 64], String> {
        let mut registers = [0_u8; 64];
        self.read(BANK, 0x00, &mut registers[..32])?;
        self.read(BANK, 0x20, &mut registers[32..])?;
        Ok(registers)
    }

    pub fn timing(&self) -> Result<Timing, String> {
        let mut r = [0_u8; 32];
        self.read(BANK, 0, &mut r)?;
        let word = |at: usize| u16::from_le_bytes([r[at], r[at + 1]]);
        Ok(Timing {
            total_height: word(4),
            total_width: word(6),
            height: word(8),
            width: word(10),
            refresh: r[12],
        })
    }

    pub fn settings(&self) -> Result<Settings, String> {
        let r = self.registers()?;
        Ok(Settings {
            colour_range: r[REG_COLOUR_RANGE as usize],
            picture: [r[0x13], r[0x14], r[0x15], r[0x16]],
            audio_gain: r[REG_AUDIO_GAIN as usize],
        })
    }

    pub fn set_picture(&self, picture: [u8; 4]) -> Result<(), String> {
        self.write(BANK, REG_PICTURE, &picture)
    }

    pub fn set_colour_range(&self, value: u8) -> Result<(), String> {
        if value > 2 {
            return Err(format!("colour range must be 0, 1 or 2, not {value}"));
        }
        self.write(BANK, REG_COLOUR_RANGE, &[value])
    }

    pub fn set_audio_gain(&self, value: u8) -> Result<(), String> {
        self.write(BANK, REG_AUDIO_GAIN, &[value])
    }

    pub fn read_edid(&self) -> Result<[u8; 256], String> {
        let mut block = [0_u8; 256];
        for offset in (0..256_u16).step_by(16) {
            let got = self.read(
                EDID_BANK,
                offset,
                &mut block[offset as usize..offset as usize + 16],
            )?;
            if got != 16 {
                return Err(format!("reading EDID bytes {offset}..: got {got} bytes"));
            }
        }
        Ok(block)
    }

    /// Writes a validated EDID and reads it back; returns the number of
    /// bytes that differ afterwards (the device may adjust fields).
    pub fn write_edid(&self, block: &[u8; 256]) -> Result<usize, String> {
        edid::validate(block)?;
        for offset in (0..256_u16).step_by(16) {
            self.write(
                EDID_BANK,
                offset,
                &block[offset as usize..offset as usize + 16],
            )?;
        }
        let back = self.read_edid()?;
        Ok(back
            .iter()
            .zip(block.iter())
            .filter(|(a, b)| a != b)
            .count())
    }

    /// Sends one of the permitted MCU status commands and returns the reply.
    ///
    /// The reply buffer keeps its previous content until the microcontroller
    /// has answered, roughly 100 ms later, and the bytes are updated one by
    /// one — so the official driver's timing is followed: wait, then poll
    /// until two reads 30 ms apart agree.
    pub fn mcu(&self, command: u8) -> Result<[u8; 3], String> {
        if MCU_FORBIDDEN.contains(&command) || !MCU_STATUS.iter().any(|(c, _)| *c == command) {
            return Err(format!("MCU command {command:#04x} is not permitted"));
        }
        self.write(MCU_PROXY, 0, &[0xab, 0x03, 0x12, 0x34, command])?;
        std::thread::sleep(Duration::from_millis(150));
        let mut reply = [0_u8; 3];
        self.read(MCU_PROXY, 0, &mut reply)?;
        for _ in 0..15 {
            std::thread::sleep(Duration::from_millis(30));
            let mut again = [0_u8; 3];
            self.read(MCU_PROXY, 0, &mut again)?;
            if again == reply {
                return Ok(reply);
            }
            reply = again;
        }
        Ok(reply)
    }

    /// The firmware build date the MCU reports to command 0x58, as
    /// "20YY-MM-DD" when it looks like one.
    pub fn firmware_date(&self) -> Result<String, String> {
        self.mcu(0x58).map(format_firmware_date)
    }
}

/// What was read from the microcontroller and the EDID EEPROM while the
/// stream was idle. Both sit behind the microcontroller, which the official
/// driver only queries before streaming; asking it while data flows has
/// hung it until the next power cycle, so a streaming process takes this
/// snapshot first and shows it afterwards.
/// One status command, what it means, and what the microcontroller answered.
pub type McuReply = (u8, &'static str, Result<[u8; 3], String>);

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub mcu: Vec<McuReply>,
    pub firmware_date: Result<String, String>,
    pub edid: Result<[u8; 256], String>,
    pub taken: Instant,
}

impl Snapshot {
    pub fn take(control: &Control) -> Self {
        let mcu = MCU_STATUS
            .iter()
            .map(|(command, meaning)| (*command, *meaning, control.mcu(*command)))
            .collect::<Vec<_>>();
        let firmware_date = mcu
            .iter()
            .find(|(command, _, _)| *command == 0x58)
            .map(|(_, _, reply)| reply.clone().map(format_firmware_date))
            .unwrap_or_else(|| Err("not read".into()));
        Self {
            mcu,
            firmware_date,
            edid: control.read_edid(),
            taken: Instant::now(),
        }
    }
}

/// "20YY-MM-DD" from the reply to command 0x58 when it looks like a date.
pub fn format_firmware_date(r: [u8; 3]) -> String {
    if (1..=12).contains(&r[1]) && (1..=31).contains(&r[2]) {
        format!("20{:02}-{:02}-{:02}", r[0], r[1], r[2])
    } else {
        format!("{:02x} {:02x} {:02x}", r[0], r[1], r[2])
    }
}

/// The plug-in sequence on any handle; see [`Control::initialise`].
pub fn initialise_handle<T: rusb::UsbContext>(handle: &DeviceHandle<T>) -> Result<(), String> {
    let out = rusb::request_type(Direction::Out, RequestType::Vendor, Recipient::Device);
    let timeout = Duration::from_secs(1);
    handle
        .write_control(out, 0xec, 0, 0, &INIT_PAYLOAD, timeout)
        .map_err(|error| format!("init request 0xec: {error}"))?;
    match handle.write_control(out, 0xc1, 0xc039, 0, &[], timeout) {
        Ok(_) | Err(rusb::Error::Pipe) => {}
        Err(error) => return Err(format!("init request 0xc1/0xc039: {error}")),
    }
    handle
        .write_control(out, 0xc1, 0x4134, 0, &[], timeout)
        .map_err(|error| format!("init request 0xc1/0x4134: {error}"))?;
    std::thread::sleep(Duration::from_millis(10));
    let mut ready = [0_u8; 1];
    let request_type = rusb::request_type(Direction::In, RequestType::Vendor, Recipient::Device);
    handle
        .read_control(request_type, 0xc1, 0x0039, 0, &mut ready, timeout)
        .map_err(|error| format!("init readback 0xc1/0x0039: {error}"))?;
    if ready[0] != 1 {
        return Err(format!(
            "init readback 0xc1/0x0039 answered {:#04x}, expected 0x01",
            ready[0]
        ));
    }
    Ok(())
}
