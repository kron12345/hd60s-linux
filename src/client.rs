//! A client for the running service: the same HTTP/JSON API the control
//! program uses, over the Unix socket (no token needed there).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hd60s-linux/api.sock")
}

/// One request; returns the status code and the body.
pub fn request(method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let socket = socket_path();
    let mut stream = UnixStream::connect(&socket).map_err(|error| {
        format!(
            "the service is not running ({error} on {}); start it with `hd60s-linux serve`, the control program or `systemctl --user start hd60s-serve.service`",
            socket.display()
        )
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(
            format!("{method} {path} HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data).map_err(|e| e.to_string())?;
    let split = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed reply from the service")?;
    let status = String::from_utf8_lossy(&data[..split])
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, data[split + 4..].to_vec()))
}

/// Minimal JSON field access without a JSON crate: the string, number or
/// boolean value of `"key":` at the top level or inside nested objects
/// (first occurrence).
pub fn field(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let at = json.find(&needle)? + needle.len();
    let rest = json[at..].trim_start();
    if let Some(s) = rest.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    if let Some(n) = chars.next() {
                        out.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                    }
                }
                '"' => break,
                c => out.push(c),
            }
        }
        Some(out)
    } else if rest.starts_with('[') {
        let end = rest.find(']')?;
        Some(rest[1..end].trim().to_string())
    } else {
        let end = rest.find([',', '}', ']']).unwrap_or(rest.len());
        Some(rest[..end].trim().to_string())
    }
}

/// The `message`, `changed` or `error` text of a JSON reply.
pub fn outcome(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    field(&text, "message")
        .or_else(|| field(&text, "changed"))
        .or_else(|| field(&text, "error"))
        .unwrap_or_else(|| text.trim().to_string())
}

/// Runs a `ctl` subcommand; prints the result.
pub fn ctl(arguments: &[String]) -> Result<(), String> {
    let post = |path: &str| -> Result<(), String> {
        let (status, body) = request("POST", path)?;
        let text = outcome(&body);
        if (200..300).contains(&status) {
            println!("{text}");
            Ok(())
        } else {
            Err(text)
        }
    };
    let value = |at: usize| -> Result<&str, String> {
        arguments
            .get(at)
            .map(String::as_str)
            .ok_or_else(|| "missing value".to_string())
    };
    match arguments.first().map(String::as_str) {
        Some("status") | None => {
            let (_, body) = request("GET", "/api/state")?;
            let json = String::from_utf8_lossy(&body);
            let get = |k: &str| field(&json, k).unwrap_or_default();
            println!(
                "device:    {}",
                if get("present") == "true" {
                    format!("{} ({})", get("revision"), get("product_id"))
                } else {
                    "none".into()
                }
            );
            println!(
                "input:     {}",
                field(&json, "text").unwrap_or_else(|| "–".into())
            );
            println!(
                "stream:    {} fps, {} frame(s), {} bad, {} format change(s)",
                get("fps"),
                get("frames"),
                get("bad"),
                get("format_changes")
            );
            println!(
                "picture:   {} (brightness/contrast/saturation/hue), range {}, gain {} ({} dB)",
                get("picture").replace(',', "/"),
                get("range_name"),
                get("gain"),
                get("gain_db")
            );
            println!(
                "recording: {}",
                if json.contains("\"recording\":null") {
                    "off".into()
                } else {
                    format!(
                        "{} ({} s, {} bytes, {} dropped)",
                        get("path"),
                        get("seconds"),
                        get("bytes"),
                        get("dropped")
                    )
                }
            );
            println!(
                "network:   {}",
                if json.contains("\"network_stream\":null") {
                    "not configured".into()
                } else {
                    format!(
                        "{} at {}",
                        if get("enabled") == "true" {
                            "on"
                        } else {
                            "off"
                        },
                        get("url")
                    )
                }
            );
            println!("encoder:   {}", get("encoder"));
            Ok(())
        }
        Some("json") => {
            let (_, body) = request("GET", "/api/state")?;
            println!("{}", String::from_utf8_lossy(&body));
            Ok(())
        }
        Some("picture") => {
            let mut query = Vec::new();
            let mut i = 1;
            while i < arguments.len() {
                let key = arguments[i].trim_start_matches("--");
                match key {
                    "brightness" | "contrast" | "saturation" | "hue" | "gain" | "range" => {
                        let v = value(i + 1)?;
                        let v = match (key, v) {
                            ("range", "bypass") => "0",
                            ("range", "shrink") => "1",
                            ("range", "expand") => "2",
                            (_, v) => v,
                        };
                        query.push(format!("{key}={v}"));
                        i += 2;
                    }
                    "reset" => {
                        query.push("reset=1".into());
                        i += 1;
                    }
                    other => return Err(format!("unknown picture option {other}")),
                }
            }
            if query.is_empty() {
                return Err("nothing to set; e.g. picture --brightness 140 --range bypass".into());
            }
            post(&format!("/api/set?{}", query.join("&")))
        }
        Some("range") => post(&format!(
            "/api/set?range={}",
            match value(1)? {
                "bypass" => "0",
                "shrink" => "1",
                "expand" => "2",
                v => v,
            }
        )),
        Some("gain") => post(&format!("/api/set?gain={}", value(1)?)),
        Some("mute") => post("/api/set?gain=0"),
        Some("unmute") => post("/api/set?gain=128"),
        Some("reset") => post("/api/set?reset=1"),
        Some("record") => match value(1)? {
            "start" => post("/api/record?start=1"),
            "stop" => post("/api/record?stop=1"),
            other => Err(format!("record start|stop, not {other}")),
        },
        Some("stream") => match value(1)? {
            "on" => post("/api/stream?on=1"),
            "off" => post("/api/stream?off=1"),
            other => Err(format!("stream on|off, not {other}")),
        },
        Some("edid") => match value(1)? {
            "restore" => post("/api/edid?restore=1"),
            "dump" => {
                let (status, body) = request("GET", "/edid.bin")?;
                if status != 200 {
                    return Err("no EDID read yet".into());
                }
                let path = value(2)?;
                std::fs::write(path, &body).map_err(|e| format!("writing {path}: {e}"))?;
                println!("saved {} bytes to {path}", body.len());
                Ok(())
            }
            other => Err(format!("edid restore|dump FILE, not {other}")),
        },
        Some("snapshot") => {
            let path = value(1)?;
            let (status, body) = request("GET", "/preview.jpg")?;
            if status != 200 {
                return Err("no frame yet".into());
            }
            std::fs::write(path, &body).map_err(|e| format!("writing {path}: {e}"))?;
            println!("saved {} bytes to {path}", body.len());
            Ok(())
        }
        Some("report") => {
            let (_, body) = request("GET", "/api/report")?;
            print!("{}", String::from_utf8_lossy(&body));
            Ok(())
        }
        Some(other) => Err(format!(
            "unknown ctl command {other}; use status|json|picture|range|gain|mute|unmute|reset|record start|stop|stream on|off|edid restore|dump FILE|snapshot FILE|report"
        )),
    }
}

/// `key = value` lines from `~/.config/hd60s-linux/serve.conf`, turned into
/// the flags `serve` understands (`panel = on` → `--panel on`). Command
/// line flags take precedence because they are searched first.
pub fn serve_config_arguments() -> Vec<String> {
    let path = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map(|d| d.join("hd60s-linux/serve.conf"));
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    parse_serve_config(&text)
}

pub fn parse_serve_config(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .flat_map(|(k, v)| {
            vec![
                format!("--{}", k.trim()),
                v.trim().trim_matches('"').to_string(),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_fields_are_found() {
        let json = r#"{"stream":{"fps":59.9,"frames":12},"device":{"present":true,"revision":"HD60 S Rev. 4"},"recording":null}"#;
        assert_eq!(field(json, "fps").as_deref(), Some("59.9"));
        assert_eq!(field(json, "present").as_deref(), Some("true"));
        assert_eq!(field(json, "revision").as_deref(), Some("HD60 S Rev. 4"));
        assert_eq!(field(json, "missing"), None);
        assert_eq!(
            field(r#"{"picture":[128,130,128,128]}"#, "picture").as_deref(),
            Some("128,130,128,128")
        );
    }

    #[test]
    fn config_lines_become_flags() {
        let flags =
            parse_serve_config("# comment\npanel = on\nstream-fps=20\nrecord-dir = \"/srv/rec\"\n");
        assert_eq!(
            flags,
            [
                "--panel",
                "on",
                "--stream-fps",
                "20",
                "--record-dir",
                "/srv/rec"
            ]
        );
    }
}
