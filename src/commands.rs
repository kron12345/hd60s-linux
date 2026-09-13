//! The direct commands: they open the card themselves (claiming the
//! streaming interface, so they refuse to run next to a service) and
//! read or write its registers through `Control`.

use std::time::{Duration, Instant};

use crate::control::{self, Control};
use crate::edid;

/// `signal [SECONDS]`: the detected input timing, once or watched.
pub fn signal(seconds: Option<u64>) -> Result<(), String> {
    let control = Control::open()?;
    let started = Instant::now();
    let mut previous = None;
    loop {
        let registers = control.registers()?;
        let block: [u8; 32] = registers[..32].try_into().unwrap();
        if previous != Some(block) {
            let raw = block
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("{}  [{raw}]", control.timing()?);
            previous = Some(block);
        }
        match seconds {
            Some(limit) if started.elapsed() < Duration::from_secs(limit) => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => return Ok(()),
        }
    }
}

/// Options for the `picture` command.
#[derive(Default, Clone)]
pub struct PictureSettings {
    pub range: Option<u8>,
    /// brightness, contrast, saturation, hue
    pub controls: [Option<u8>; 4],
    pub reset: bool,
}

/// `picture`: shows or changes the colour range and the picture controls.
pub fn picture(settings: PictureSettings) -> Result<(), String> {
    let control = Control::open()?;
    let mut controls = control.settings()?.picture;
    let mut changed = settings.reset;
    if settings.reset {
        controls = [0x80; 4];
    }
    for (slot, value) in controls.iter_mut().zip(settings.controls) {
        if let Some(value) = value {
            *slot = value;
            changed = true;
        }
    }
    if changed {
        control.set_picture(controls)?;
    }
    if let Some(range) = settings.range {
        control.set_colour_range(range)?;
    }
    let now = control.settings()?;
    println!(
        "colour range: {} ({:#04x})",
        match now.colour_range {
            0 => "bypass",
            1 => "shrink (the application calls this expanded)",
            2 => "expand",
            _ => "unset (power-on default)",
        },
        now.colour_range
    );
    for (name, value) in ["brightness", "contrast", "saturation", "hue"]
        .iter()
        .zip(now.picture)
    {
        println!(
            "{name:<11} {value:>3}{}",
            if value == 0x80 { "  (neutral)" } else { "" }
        );
    }
    Ok(())
}

/// `audio [--gain N|--mute]`: shows or sets the audio gain.
pub fn audio(gain: Option<u8>) -> Result<(), String> {
    let control = Control::open()?;
    if let Some(gain) = gain {
        control.set_audio_gain(gain)?;
    }
    let gain = control.settings()?.audio_gain;
    if gain == 0 {
        println!("audio gain: 0 (muted)");
    } else {
        println!(
            "audio gain: {gain} ({:+.1} dB relative to 0x80)",
            control::gain_db(gain)
        );
    }
    Ok(())
}

/// Options for the `edid` command.
#[derive(Default, Clone)]
pub struct EdidSettings {
    pub dump: Option<String>,
    pub write: Option<String>,
    pub restore: bool,
    pub fix: bool,
}

/// `edid`: reads, saves, writes or restores the EDID the card presents to
/// its HDMI source. A block that fails validation is refused unless `--fix`
/// recomputes its checksums; the source re-reads it on the next hot-plug.
pub fn edid(settings: EdidSettings) -> Result<(), String> {
    let control = Control::open()?;
    let current = control.read_edid()?;
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
        let differing = control.write_edid(&block)?;
        let back = control.read_edid()?;
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

/// `mcu`: the three microcontroller status queries.
pub fn mcu() -> Result<(), String> {
    let control = Control::open()?;
    for (command, meaning) in control::MCU_STATUS {
        let reply = control.mcu(command)?;
        println!(
            "command {command:#04x} ({meaning}): reply {:02x} {:02x} {:02x}",
            reply[0], reply[1], reply[2]
        );
    }
    println!("firmware build date: {}", control.firmware_date()?);
    Ok(())
}
