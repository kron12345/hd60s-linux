//! The service's API and web panel: `GET /api/state` returns the
//! [`hd60s_api::State`], `POST /api/...` changes things, and a few binary
//! endpoints hand out the picture and the EDID. The same handler serves
//! the Unix socket (trusted: same user) and, when switched on, a TCP port
//! for browsers and remote scripts (guarded by a token and Host/Origin
//! checks). The embedded page is a web version of the control program.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use hd60s_api::{Outcome, State};

use crate::control;
use crate::edid;
use crate::http::{Request, host_allowed, parse, respond, respond_json};
use crate::report;
use crate::serve::Shared;

const PAGE: &str = include_str!("panel.html");

/// The web panel on a TCP address.
pub fn run(address: &str, shared: Arc<Shared>) -> Result<(), String> {
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("binding the panel to {address}: {error}"))?;
    eprintln!("control panel at http://{address}/");
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || handle(stream, shared, false));
    }
    Ok(())
}

/// Serves the same API on a Unix socket for programs of the same user
/// (`hd60s-control`); the socket's permissions replace the token.
pub fn run_unix(path: &std::path::Path, shared: Arc<Shared>) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    if let Some(dir) = path.parent() {
        let _ = std::fs::DirBuilder::new().mode(0o700).create(dir);
    }
    // Never take the socket from a live instance (a second `serve` would
    // otherwise cut the first one off from its clients).
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        return Err(format!(
            "another instance already serves {}; this one offers no API socket",
            path.display()
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)
        .map_err(|error| format!("binding the API socket {}: {error}", path.display()))?;
    eprintln!("API socket at {}", path.display());
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || handle(stream, shared, true));
    }
    Ok(())
}

/// Everything a client wants to know, read from the shared state and, for
/// timing and settings, from the card through the streaming handle.
pub fn state(shared: &Shared) -> State {
    let stats = *shared.stats.lock().unwrap();
    let geometry = *shared.geometry.lock().unwrap();
    let mut state = State {
        stream: hd60s_api::StreamStats {
            frames: stats.frames,
            bad: stats.bad_frames,
            format_changes: stats.format_changes,
            audio_blocks: stats.audio_blocks,
            fps: shared.recent_fps(),
            present: shared.device_present.load(Ordering::Relaxed),
            geometry: [geometry.0, geometry.1],
            uptime: shared.started.elapsed().as_secs(),
        },
        message: shared.message.lock().unwrap().clone(),
        recording: shared
            .recording_status()
            .map(|s| hd60s_api::RecordingStatus {
                path: s.path.display().to_string(),
                seconds: s.seconds,
                bytes: s.bytes,
                dropped: s.dropped_frames,
            }),
        record_dir: shared.record_dir.display().to_string(),
        encoder: shared
            .resolved_encoder
            .lock()
            .unwrap()
            .map(|e| e.name())
            .unwrap_or(shared.record_encoder.name())
            .to_string(),
        network_stream: shared
            .stream_bind
            .as_ref()
            .map(|bind| hd60s_api::NetworkStream {
                enabled: shared.network_stream.load(Ordering::Relaxed),
                url: format!(
                    "http://{}:{}/stream.mjpg",
                    hostname(),
                    bind.rsplit(':').next().unwrap_or("8061")
                ),
                clients: shared.stream_clients.load(Ordering::Relaxed),
                fps: shared.stream_fps,
                scale: shared.stream_scale,
            }),
        ..Default::default()
    };
    let control = shared.control.lock().unwrap();
    let snapshot = shared.snapshot.lock().unwrap();
    match control.as_ref() {
        Some(control) => {
            state.device = hd60s_api::Device {
                present: true,
                error: None,
                revision: control.revision.to_string(),
                product_id: format!("0fd9:{:04x}", control.product_id),
                firmware: control.device_version.clone(),
                speed: format!("{:?}", control.speed),
                bus: control.bus,
                address: control.address,
                mcu_build: snapshot
                    .as_ref()
                    .map(|s| s.firmware_date.clone().unwrap_or_else(|e| e))
                    .unwrap_or_else(|| "not read".into()),
            };
            match control.timing() {
                Ok(t) => {
                    state.timing = Some(hd60s_api::Timing {
                        present: t.present(),
                        width: t.width,
                        height: t.height,
                        total_width: t.total_width,
                        total_height: t.total_height,
                        refresh: t.refresh,
                        text: t.to_string(),
                    })
                }
                Err(e) => state.timing_error = Some(e),
            }
            match control.settings() {
                Ok(s) => {
                    state.settings = Some(hd60s_api::Settings {
                        range: s.colour_range,
                        range_name: control::colour_range_name(s.colour_range).to_string(),
                        picture: s.picture,
                        gain: s.audio_gain,
                        gain_db: control::gain_db(s.audio_gain),
                    })
                }
                Err(e) => state.settings_error = Some(e),
            }
        }
        None => {
            state.device.error = Some("no HD60 S attached".into());
        }
    }
    if let Some(snapshot) = snapshot.as_ref() {
        match &snapshot.edid {
            Ok(block) => {
                state.edid = Some(hd60s_api::Edid {
                    summary: edid::summary(block),
                    valid: edid::validate(block).is_ok(),
                    factory: *block == edid::FACTORY,
                })
            }
            Err(e) => state.edid_error = Some(e.clone()),
        }
        state.mcu = snapshot
            .mcu
            .iter()
            .map(|(command, meaning, reply)| hd60s_api::McuReply {
                command: format!("{command:#04x}"),
                meaning: meaning.to_string(),
                reply: match reply {
                    Ok(r) => format!("{:02x} {:02x} {:02x}", r[0], r[1], r[2]),
                    Err(e) => e.clone(),
                },
            })
            .collect();
        state.snapshot_age = Some(snapshot.taken.elapsed().as_secs());
    }
    state
}

fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "localhost".into())
}

/// `POST /api/set`: picture registers, colour range, gain, or a reset.
fn apply(shared: &Shared, request: &Request) -> Result<String, String> {
    let control = shared.control.lock().unwrap();
    let control = control.as_ref().ok_or("no HD60 S attached")?;
    let mut changed = Vec::new();
    if request.get("reset").is_some() {
        control.set_picture([0x80; 4])?;
        control.set_audio_gain(0x80)?;
        control.set_colour_range(0)?;
        changed.push("reset".to_string());
    }
    const KEYS: [&str; 4] = ["brightness", "contrast", "saturation", "hue"];
    if KEYS.iter().any(|key| request.number(key).is_some()) {
        let mut picture = control.settings()?.picture;
        for (index, key) in KEYS.iter().enumerate() {
            if let Some(value) = request.number(key) {
                picture[index] = value;
                changed.push(format!("{key}={value}"));
            }
        }
        control.set_picture(picture)?;
    }
    if let Some(range) = request.number("range") {
        control.set_colour_range(range)?;
        changed.push(format!("range={range}"));
    }
    if let Some(gain) = request.number("gain") {
        control.set_audio_gain(gain)?;
        changed.push(format!("gain={gain}"));
    }
    Ok(changed.join(" "))
}

fn preview(shared: &Shared, factor: usize, quality: u8) -> Option<Vec<u8>> {
    let frame = shared.latest.lock().unwrap().clone()?;
    jpeg_from_yuyv(&frame, factor, quality)
}

/// A 1920x1080 YUYV frame as JPEG, at full size or reduced by an integer
/// factor with a box filter (BT.709, limited range).
pub fn jpeg_from_yuyv(frame: &[u8], factor: usize, quality: u8) -> Option<Vec<u8>> {
    const SRC_W: usize = crate::frame::MAX_WIDTH;
    const SRC_H: usize = crate::frame::MAX_HEIGHT;
    if frame.len() != SRC_W * SRC_H * 2 {
        return None;
    }
    let factor = factor.clamp(1, 8);
    let (w, h) = (SRC_W / factor, SRC_H / factor);
    let mut rgb = vec![0_u8; w * h * 3];
    let samples = (factor * factor) as f32;
    for y in 0..h {
        for x in 0..w {
            let (mut sy, mut su, mut sv) = (0.0_f32, 0.0_f32, 0.0_f32);
            for dy in 0..factor {
                let row = &frame[(y * factor + dy) * SRC_W * 2..];
                for dx in 0..factor {
                    let sx = x * factor + dx;
                    let pair = (sx / 2) * 4;
                    sy += row[pair + if sx.is_multiple_of(2) { 0 } else { 2 }] as f32;
                    su += row[pair + 1] as f32;
                    sv += row[pair + 3] as f32;
                }
            }
            let luma = (sy / samples - 16.0) * 1.164;
            let u = su / samples - 128.0;
            let v = sv / samples - 128.0;
            let at = (y * w + x) * 3;
            rgb[at] = (luma + 1.793 * v).clamp(0.0, 255.0) as u8;
            rgb[at + 1] = (luma - 0.213 * u - 0.533 * v).clamp(0.0, 255.0) as u8;
            rgb[at + 2] = (luma + 2.112 * u).clamp(0.0, 255.0) as u8;
        }
    }
    let mut out = Vec::with_capacity(w * h / 2);
    let encoder = jpeg_encoder::Encoder::new(&mut out, quality.clamp(30, 100));
    encoder
        .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(out)
}

/// One connection. `trusted` (the Unix socket) skips the token and the
/// Host and Origin checks that protect the TCP port from browsers.
fn handle<S: Read + Write>(mut stream: S, shared: Arc<Shared>, trusted: bool) {
    let Some(request) = parse(&mut stream) else {
        return;
    };
    if !trusted
        && let Some(bind) = &shared.panel_bind
        && !host_allowed(&request, bind)
    {
        respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"unexpected Host header",
        );
        return;
    }
    if !trusted && request.method != "GET" && !request.authorised(&shared.panel_token) {
        respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"missing or wrong token: send it as X-Token or ?token=; scripts find it in $XDG_RUNTIME_DIR/hd60s-linux/token",
        );
        return;
    }
    // A change either succeeds with a message or fails with a reason.
    let answer = |stream: &mut S, result: Result<String, String>| match result {
        Ok(message) => respond_json(stream, "200 OK", &Outcome::ok(message)),
        Err(error) => respond_json(stream, "500 Internal Server Error", &Outcome::error(error)),
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.replace("__TOKEN__", &shared.panel_token).as_bytes(),
        ),
        ("GET", "/api/state") => respond_json(&mut stream, "200 OK", &state(&shared)),
        ("POST", "/api/set") => match apply(&shared, &request) {
            Ok(changed) => respond_json(&mut stream, "200 OK", &Outcome::changed(changed)),
            Err(error) => respond_json(
                &mut stream,
                "500 Internal Server Error",
                &Outcome::error(error),
            ),
        },
        ("POST", "/api/edid") => {
            let result = if request.get("restore").is_some() {
                shared.request_edid_write(edid::FACTORY).map(|()| {
                    "power-on EDID scheduled; the stream restarts for a moment".to_string()
                })
            } else {
                Err("nothing to do".into())
            };
            answer(&mut stream, result);
        }
        ("POST", "/api/record") => {
            let result = if request.get("start").is_some() {
                shared
                    .start_recording()
                    .map(|s| format!("recording to {}", s.path.display()))
            } else if request.get("stop").is_some() {
                shared.stop_recording().map(|s| {
                    format!(
                        "saved {} ({:.0} s, {} MB, {} frame(s) dropped)",
                        s.path.display(),
                        s.seconds,
                        s.bytes / 1_000_000,
                        s.dropped_frames
                    )
                })
            } else {
                Err("start=1 or stop=1".into())
            };
            answer(&mut stream, result);
        }
        ("POST", "/api/stream") => {
            let on = request.get("on").is_some();
            shared.network_stream.store(on, Ordering::Relaxed);
            eprintln!("network stream switched {}", if on { "on" } else { "off" });
            answer(
                &mut stream,
                Ok(format!("network stream {}", if on { "on" } else { "off" })),
            );
        }
        ("GET", "/edid.bin") => {
            let block = shared
                .snapshot
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|s| s.edid.clone().ok());
            match block {
                Some(block) => respond(&mut stream, "200 OK", "application/octet-stream", &block),
                None => respond(
                    &mut stream,
                    "404 Not Found",
                    "text/plain",
                    b"no EDID read yet",
                ),
            }
        }
        ("GET", "/frame.yuyv") => {
            let frame = shared.latest.lock().unwrap().clone();
            match frame {
                Some(frame) => respond(&mut stream, "200 OK", "application/octet-stream", &frame),
                None => respond(&mut stream, "204 No Content", "text/plain", b""),
            }
        }
        ("GET", "/preview.jpg") => match preview(
            &shared,
            request.number("scale").map(usize::from).unwrap_or(1),
            request.number("quality").unwrap_or(88),
        ) {
            Some(jpeg) => respond(&mut stream, "200 OK", "image/jpeg", &jpeg),
            None => respond(&mut stream, "204 No Content", "text/plain", b""),
        },
        ("GET", "/api/report") => {
            let text = report::from_shared(&shared, request.get("serial").is_some());
            respond(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                text.as_bytes(),
            )
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}
