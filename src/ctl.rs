//! `hd60s-linux ctl`: the running service from the shell, through the
//! Unix socket (no token there).

use hd60s_api::{self as api, Command};

fn send(command: Command) -> Result<(), String> {
    let outcome = api::send(&command);
    if outcome.ok {
        println!("{}", outcome.text());
        Ok(())
    } else {
        Err(outcome.text())
    }
}

fn range_value(name: &str) -> Result<u8, String> {
    match name {
        "bypass" => Ok(0),
        "shrink" => Ok(1),
        "expand" => Ok(2),
        other => other
            .parse()
            .map_err(|_| format!("colour range: bypass, shrink, expand or 0-2, not {other}")),
    }
}

fn number(arguments: &[String], at: usize) -> Result<u8, String> {
    arguments
        .get(at)
        .ok_or("missing value")?
        .parse()
        .map_err(|_| format!("{} is not a number 0-255", arguments[at]))
}

fn save(path: &str, endpoint: &str, what: &str) -> Result<(), String> {
    let (status, body) = api::request("GET", endpoint)?;
    if status != 200 {
        return Err(format!("no {what} available yet"));
    }
    std::fs::write(path, &body).map_err(|e| format!("writing {path}: {e}"))?;
    println!("saved {} bytes to {path}", body.len());
    Ok(())
}

pub fn run(arguments: &[String]) -> Result<(), String> {
    let word = |at: usize| arguments.get(at).map(String::as_str).unwrap_or("");
    match word(0) {
        "status" | "" => {
            let s = api::state()?;
            println!(
                "device:    {}",
                if s.device.present {
                    format!("{} ({})", s.device.revision, s.device.product_id)
                } else {
                    s.device.error.unwrap_or_else(|| "none".into())
                }
            );
            println!(
                "input:     {}",
                s.timing
                    .map(|t| t.text)
                    .or(s.timing_error)
                    .unwrap_or_else(|| "–".into())
            );
            println!(
                "stream:    {:.1} fps, {} frame(s), {} bad, {} format change(s)",
                s.stream.fps, s.stream.frames, s.stream.bad, s.stream.format_changes
            );
            if let Some(p) = &s.settings {
                println!(
                    "picture:   {}/{}/{}/{} (brightness/contrast/saturation/hue), range {}, gain {} ({:+.1} dB)",
                    p.picture[0],
                    p.picture[1],
                    p.picture[2],
                    p.picture[3],
                    p.range_name,
                    p.gain,
                    p.gain_db
                );
            }
            println!(
                "recording: {}",
                match &s.recording {
                    Some(r) => format!(
                        "{} ({:.0} s, {} MB, {} dropped)",
                        r.path,
                        r.seconds,
                        r.bytes / 1_000_000,
                        r.dropped
                    ),
                    None => format!("off (files go to {}, encoder {})", s.record_dir, s.encoder),
                }
            );
            println!(
                "network:   {}",
                match &s.network_stream {
                    Some(n) => format!(
                        "{} at {} ({} client(s))",
                        if n.enabled { "on" } else { "off" },
                        n.url,
                        n.clients
                    ),
                    None => "not configured".into(),
                }
            );
            if let Some(message) = s.message {
                println!("message:   {message}");
            }
            Ok(())
        }
        "json" => {
            let (_, body) = api::request("GET", "/api/state")?;
            println!("{}", String::from_utf8_lossy(&body));
            Ok(())
        }
        "picture" => {
            let mut i = 1;
            let mut sent = 0;
            while i < arguments.len() {
                let key = arguments[i].trim_start_matches("--");
                match key {
                    "brightness" | "contrast" | "saturation" | "hue" => {
                        let key: &'static str = ["brightness", "contrast", "saturation", "hue"]
                            .into_iter()
                            .find(|k| *k == key)
                            .unwrap();
                        send(Command::Picture {
                            key,
                            value: number(arguments, i + 1)?,
                        })?;
                        i += 2;
                    }
                    "range" => {
                        send(Command::Range(range_value(word(i + 1))?))?;
                        i += 2;
                    }
                    "gain" => {
                        send(Command::Gain(number(arguments, i + 1)?))?;
                        i += 2;
                    }
                    "reset" => {
                        send(Command::ResetPicture)?;
                        i += 1;
                    }
                    other => return Err(format!("unknown picture option {other}")),
                }
                sent += 1;
            }
            if sent == 0 {
                return Err("nothing to set; e.g. picture --brightness 140 --range bypass".into());
            }
            Ok(())
        }
        "range" => send(Command::Range(range_value(word(1))?)),
        "gain" => send(Command::Gain(number(arguments, 1)?)),
        "mute" => send(Command::Gain(0)),
        "unmute" => send(Command::Gain(128)),
        "reset" => send(Command::ResetPicture),
        "record" => match word(1) {
            "start" => send(Command::RecordStart),
            "stop" => send(Command::RecordStop),
            other => Err(format!("record start|stop, not {other:?}")),
        },
        "stream" => match word(1) {
            "on" => send(Command::StreamOn),
            "off" => send(Command::StreamOff),
            other => Err(format!("stream on|off, not {other:?}")),
        },
        "edid" => match word(1) {
            "restore" => send(Command::RestoreEdid),
            "dump" if !word(2).is_empty() => save(word(2), "/edid.bin", "EDID"),
            _ => Err("edid restore | edid dump FILE".into()),
        },
        "snapshot" if !word(1).is_empty() => save(word(1), "/preview.jpg", "frame"),
        "report" => {
            let (_, body) = api::request("GET", "/api/report")?;
            print!("{}", String::from_utf8_lossy(&body));
            Ok(())
        }
        other => Err(format!(
            "unknown ctl command {other:?}; use status|json|picture|range|gain|mute|unmute|reset|record start|stop|stream on|off|edid restore|dump FILE|snapshot FILE|report"
        )),
    }
}
