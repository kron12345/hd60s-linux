//! A diagnostic report to paste into an issue: what the tool, the system and
//! the device look like, and whether a short capture works. The serial
//! number is left out unless asked for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::control::{self, Control, Snapshot};
use crate::device;
use crate::edid;
use crate::pump::{self, Input};
use crate::serve::Shared;

fn os_release() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                line.strip_prefix("PRETTY_NAME=")
                    .map(|v| v.trim_matches('"').to_string())
            })
        })
        .unwrap_or_else(|| "unknown distribution".into())
}

fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown kernel".into())
}

fn pipewire_version() -> String {
    std::process::Command::new("pipewire")
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Linked with libpipewire ")
                        .map(str::to_string)
                })
        })
        .unwrap_or_else(|| "not found".into())
}

fn speed_name(speed: rusb::Speed) -> &'static str {
    match speed {
        rusb::Speed::Low => "Low Speed (1.5 Mbps)",
        rusb::Speed::Full => "Full Speed (12 Mbps)",
        rusb::Speed::High => "High Speed (480 Mbps) — too slow for video",
        rusb::Speed::Super => "SuperSpeed (5 Gbps)",
        rusb::Speed::SuperPlus => "SuperSpeed+ (10 Gbps)",
        _ => "unknown",
    }
}

fn header() -> Vec<String> {
    vec![
        "## hd60s-linux report".to_string(),
        format!("- tool: hd60s-linux {}", env!("CARGO_PKG_VERSION")),
        format!(
            "- system: {}, kernel {}, PipeWire {}",
            os_release(),
            kernel(),
            pipewire_version()
        ),
    ]
}

/// The device part: identity, snapshot (MCU, EDID), live timing and settings.
pub fn describe(control: &Control, snapshot: &Snapshot, show_serial: bool) -> Vec<String> {
    let mut lines = Vec::new();
    let mut device_line = format!(
        "- device: {} (0fd9:{:04x}), firmware {}, USB {} at bus {} address {}",
        control.revision,
        control.product_id,
        control.device_version,
        speed_name(control.speed),
        control.bus,
        control.address
    );
    if show_serial && let Some(serial) = &control.serial {
        device_line.push_str(&format!(", serial {serial}"));
    }
    lines.push(device_line);
    lines.push("- access: opened without root".to_string());
    lines.push(format!(
        "- MCU firmware build: {}",
        snapshot.firmware_date.clone().unwrap_or_else(|error| error)
    ));
    let mcu = snapshot
        .mcu
        .iter()
        .map(|(command, meaning, reply)| match reply {
            Ok(r) => format!(
                "{command:#04x} {meaning} → {:02x} {:02x} {:02x}",
                r[0], r[1], r[2]
            ),
            Err(error) => format!("{command:#04x} {meaning} → {error}"),
        })
        .collect::<Vec<_>>();
    lines.push(format!("- MCU: {}", mcu.join(", ")));
    match &snapshot.edid {
        Ok(block) => lines.push(format!(
            "- EDID: {}; {}{}",
            edid::summary(block),
            match edid::validate(block) {
                Ok(()) => "valid".to_string(),
                Err(error) => format!("invalid: {error}"),
            },
            if *block == edid::FACTORY {
                " (power-on block)"
            } else {
                " (custom)"
            }
        )),
        Err(error) => lines.push(format!("- EDID: {error}")),
    }
    match control.timing() {
        Ok(timing) => lines.push(format!("- input: {timing}")),
        Err(error) => lines.push(format!("- input: {error}")),
    }
    match control.settings() {
        Ok(s) => lines.push(format!(
            "- settings: colour range {} ({}), picture {}/{}/{}/{} (brightness/contrast/saturation/hue, 128 neutral), audio gain {} ({:+.1} dB)",
            s.colour_range,
            control::colour_range_name(s.colour_range),
            s.picture[0],
            s.picture[1],
            s.picture[2],
            s.picture[3],
            s.audio_gain,
            control::gain_db(s.audio_gain)
        )),
        Err(error) => lines.push(format!("- settings: {error}")),
    }
    lines
}

/// The command-line report: opens the device exclusively, reads everything,
/// then (with `capture_seconds` > 0) streams briefly to prove the data path.
pub fn text(show_serial: bool, capture_seconds: u64) -> String {
    let mut lines = header();
    match Control::open() {
        Ok(control) => {
            let snapshot = Snapshot::take(&control);
            lines.extend(describe(&control, &snapshot, show_serial));
            lines.push(format!(
                "- kernel driver on the streaming interface: {}",
                match control.kernel_driver_active() {
                    Some(true) => "yes (detached while this tool streams)",
                    Some(false) => "no",
                    None => "unknown",
                }
            ));
            drop(control);
            if capture_seconds > 0 {
                lines.push(capture_line(capture_seconds));
            }
        }
        Err(error) => {
            lines.push(format!("- device: {error}"));
            lines.push(format!("- looked for: {}", device::known_ids()));
        }
    }
    lines.join("\n") + "\n"
}

/// The same report from inside `serve`, with the live stream statistics in
/// place of a test capture.
pub fn from_shared(shared: &Shared, show_serial: bool) -> String {
    let mut lines = header();
    let control = shared.control.lock().unwrap();
    let snapshot = shared.snapshot.lock().unwrap();
    match (control.as_ref(), snapshot.as_ref()) {
        (Some(control), Some(snapshot)) => lines.extend(describe(control, snapshot, show_serial)),
        _ => lines.push("- device: not attached".to_string()),
    }
    let stats = *shared.stats.lock().unwrap();
    let geometry = *shared.geometry.lock().unwrap();
    lines.push(format!(
        "- stream: {}, {:.1} fps now, {} frame(s), {} bad, {} format change(s), {} audio block(s), source {}x{}, up {} s",
        if shared.device_present.load(Ordering::Relaxed) { "running" } else { "waiting for the card" },
        shared.recent_fps(),
        stats.frames,
        stats.bad_frames,
        stats.format_changes,
        stats.audio_blocks,
        geometry.0,
        geometry.1,
        shared.started.elapsed().as_secs()
    ));
    lines.join("\n") + "\n"
}

fn capture_line(seconds: u64) -> String {
    let context = match rusb::Context::new() {
        Ok(context) => context,
        Err(error) => return format!("- capture: {error}"),
    };
    let Some(device) = device::find(&context) else {
        return "- capture: device disappeared".to_string();
    };
    let handle = match device.open() {
        Ok(handle) => handle,
        Err(error) => return format!("- capture: opening device: {error}"),
    };
    let started = Instant::now();
    let mut geometry = None;
    let result = pump::run(
        Input::Usb(Arc::new(handle)),
        Arc::new(AtomicBool::new(false)),
        Some(seconds),
        |frame| geometry = Some((frame.width, frame.height)),
        |_| {},
    );
    match result {
        Ok(stats) => format!(
            "- capture ({seconds} s): {} frame(s), {:.1} fps, {} bad, {} format change(s), {} audio block(s), source {}",
            stats.frames,
            stats.frames as f64 / started.elapsed().as_secs_f64(),
            stats.bad_frames,
            stats.format_changes,
            stats.audio_blocks,
            geometry
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_else(|| "none".into())
        ),
        Err(error) => format!("- capture: {error}"),
    }
}
