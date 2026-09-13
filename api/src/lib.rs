//! What the service tells its clients, and how they talk to it.
//!
//! The service (`hd60s-linux serve`) answers `GET /api/state` with a
//! [`State`]; the command line (`hd60s-linux ctl`) and the control program
//! read the same type back. Changes are `POST`s with a query string, see
//! [`Command`]. Locally everything goes over a Unix socket in
//! `$XDG_RUNTIME_DIR/hd60s-linux/`, which only the same user can reach.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

pub use serde;
use serde::{Deserialize, Serialize};

/// Everything the panel, the tray and the program show.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct State {
    pub stream: StreamStats,
    pub device: Device,
    /// Detected input timing; `None` while no device is attached or the
    /// registers could not be read (then `timing_error` says why).
    pub timing: Option<Timing>,
    pub timing_error: Option<String>,
    pub settings: Option<Settings>,
    pub settings_error: Option<String>,
    pub edid: Option<Edid>,
    pub edid_error: Option<String>,
    pub mcu: Vec<McuReply>,
    /// Seconds since the microcontroller and EDID snapshot was taken.
    pub snapshot_age: Option<u64>,
    pub recording: Option<RecordingStatus>,
    pub record_dir: String,
    pub encoder: String,
    pub network_stream: Option<NetworkStream>,
    /// Outcome of the last deferred job (an EDID write, a recorder that
    /// stopped by itself).
    pub message: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StreamStats {
    pub frames: u64,
    pub bad: u64,
    pub format_changes: u64,
    pub audio_blocks: u64,
    /// Over the last five seconds.
    pub fps: f64,
    /// Whether the service currently streams from a card (or a recording).
    pub present: bool,
    /// Width and height of the source before letterboxing.
    pub geometry: [usize; 2],
    pub uptime: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Device {
    pub present: bool,
    /// Why there is no device, when there is none.
    pub error: Option<String>,
    pub revision: String,
    pub product_id: String,
    pub firmware: String,
    pub speed: String,
    pub bus: u8,
    pub address: u8,
    pub mcu_build: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Timing {
    pub present: bool,
    pub width: u16,
    pub height: u16,
    pub total_width: u16,
    pub total_height: u16,
    pub refresh: u8,
    pub text: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Settings {
    pub range: u8,
    pub range_name: String,
    /// brightness, contrast, saturation, hue; 128 is neutral
    pub picture: [u8; 4],
    pub gain: u8,
    pub gain_db: f64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Edid {
    pub summary: String,
    pub valid: bool,
    /// Whether it is the block the card had at power-on.
    pub factory: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct McuReply {
    pub command: String,
    pub meaning: String,
    pub reply: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct RecordingStatus {
    pub path: String,
    pub seconds: f64,
    pub bytes: u64,
    pub dropped: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct NetworkStream {
    pub enabled: bool,
    pub url: String,
    pub clients: usize,
    pub fps: u32,
    pub scale: usize,
}

/// Reply to a `POST`.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Outcome {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Outcome {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            ..Default::default()
        }
    }
    pub fn changed(changed: impl Into<String>) -> Self {
        Self {
            ok: true,
            changed: Some(changed.into()),
            ..Default::default()
        }
    }
    pub fn error(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            ..Default::default()
        }
    }
    /// The one line worth showing.
    pub fn text(&self) -> String {
        self.message
            .clone()
            .or_else(|| self.changed.clone())
            .or_else(|| self.error.clone())
            .unwrap_or_default()
    }
}

/// The changes a client can ask for, and the request each one becomes.
#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Picture { key: &'static str, value: u8 },
    Range(u8),
    Gain(u8),
    ResetPicture,
    RecordStart,
    RecordStop,
    StreamOn,
    StreamOff,
    RestoreEdid,
}

impl Command {
    pub fn path(&self) -> String {
        match self {
            Command::Picture { key, value } => format!("/api/set?{key}={value}"),
            Command::Range(v) => format!("/api/set?range={v}"),
            Command::Gain(v) => format!("/api/set?gain={v}"),
            Command::ResetPicture => "/api/set?reset=1".into(),
            Command::RecordStart => "/api/record?start=1".into(),
            Command::RecordStop => "/api/record?stop=1".into(),
            Command::StreamOn => "/api/stream?on=1".into(),
            Command::StreamOff => "/api/stream?off=1".into(),
            Command::RestoreEdid => "/api/edid?restore=1".into(),
        }
    }
}

/// Where the service's Unix socket and token live.
pub fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("hd60s-linux")
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("api.sock")
}

/// One HTTP request over the Unix socket; status code and body.
pub fn request(method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let socket = socket_path();
    let mut stream = UnixStream::connect(&socket).map_err(|error| {
        format!(
            "the service is not running ({error} on {})",
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

pub fn state() -> Result<State, String> {
    let (_, body) = request("GET", "/api/state")?;
    serde_json::from_slice(&body).map_err(|e| format!("reading the state: {e}"))
}

/// Sends a command; the outcome's text is what to show.
pub fn send(command: &Command) -> Outcome {
    post(&command.path())
}

pub fn post(path: &str) -> Outcome {
    match request("POST", path) {
        Ok((_, body)) => serde_json::from_slice(&body)
            .unwrap_or_else(|_| Outcome::error(String::from_utf8_lossy(&body).into_owned())),
        Err(error) => Outcome::error(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let state = State {
            timing: Some(Timing {
                present: true,
                width: 1920,
                height: 1080,
                ..Default::default()
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(back.timing.unwrap().width, 1920);
        assert!(back.recording.is_none());
    }

    #[test]
    fn commands_become_paths() {
        assert_eq!(
            Command::Picture {
                key: "hue",
                value: 3
            }
            .path(),
            "/api/set?hue=3"
        );
        assert_eq!(Command::RecordStop.path(), "/api/record?stop=1");
    }
}
