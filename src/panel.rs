//! The control panel: a small HTTP server inside `serve`, bound to
//! localhost, with a single page that shows the input, a live preview, the
//! picture controls, colour range, audio gain, EDID, MCU status and the
//! stream statistics — and lets the user change what can be changed.
//!
//! Plain HTTP/1.1 on `std::net`, one thread per connection, no framework:
//! the page is embedded, the API answers JSON, the preview is a JPEG of the
//! latest frame. Register access opens its own control connection per
//! request; that needs no interface claim, so it works while streaming.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::control;
use crate::edid;
use crate::report;
use crate::serve::Shared;

const PAGE: &str = include_str!("panel.html");

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
        std::thread::spawn(move || handle(stream, shared));
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
}

impl Request {
    fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn number(&self, key: &str) -> Option<u8> {
        self.get(key).and_then(|v| v.parse::<u8>().ok())
    }
}

fn parse(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = [0_u8; 8192];
    let mut data = Vec::new();
    loop {
        let n = stream.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        data.extend_from_slice(&buffer[..n]);
        if data.windows(4).any(|w| w == b"\r\n\r\n") || data.len() > 65536 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&data);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect();
    Some(Request {
        method,
        path: path.to_string(),
        query,
    })
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn respond(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn state_json(shared: &Shared) -> String {
    let mut fields = Vec::new();
    let stats = *shared.stats.lock().unwrap();
    let uptime = shared.started.elapsed().as_secs_f64();
    let geometry = *shared.geometry.lock().unwrap();
    fields.push(format!(
        "\"stream\":{{\"frames\":{},\"bad\":{},\"format_changes\":{},\"audio_blocks\":{},\"fps\":{:.1},\"present\":{},\"geometry\":[{},{}],\"uptime\":{:.0}}}",
        stats.frames,
        stats.bad_frames,
        stats.format_changes,
        stats.audio_blocks,
        shared.recent_fps(),
        shared.device_present.load(Ordering::Relaxed),
        geometry.0,
        geometry.1,
        uptime
    ));
    if let Some(message) = shared.message.lock().unwrap().as_ref() {
        fields.push(format!("\"message\":{}", json_string(message)));
    }
    let control = shared.control.lock().unwrap();
    let snapshot = shared.snapshot.lock().unwrap();
    match control.as_ref() {
        Some(control) => {
            fields.push(format!(
                "\"device\":{{\"present\":true,\"revision\":{},\"product_id\":\"0fd9:{:04x}\",\"firmware\":{},\"speed\":{},\"bus\":{},\"address\":{},\"mcu_build\":{}}}",
                json_string(control.revision),
                control.product_id,
                json_string(&control.device_version),
                json_string(&format!("{:?}", control.speed)),
                control.bus,
                control.address,
                json_string(
                    &snapshot
                        .as_ref()
                        .map(|s| s.firmware_date.clone().unwrap_or_else(|e| e))
                        .unwrap_or_else(|| "not read".into())
                )
            ));
            match control.timing() {
                Ok(t) => fields.push(format!(
                    "\"timing\":{{\"present\":{},\"width\":{},\"height\":{},\"total_width\":{},\"total_height\":{},\"refresh\":{},\"text\":{}}}",
                    t.present(),
                    t.width,
                    t.height,
                    t.total_width,
                    t.total_height,
                    t.refresh,
                    json_string(&t.to_string())
                )),
                Err(e) => fields.push(format!("\"timing\":{{\"error\":{}}}", json_string(&e))),
            }
            match control.settings() {
                Ok(s) => fields.push(format!(
                    "\"settings\":{{\"range\":{},\"range_name\":{},\"picture\":[{},{},{},{}],\"gain\":{},\"gain_db\":{:.1}}}",
                    s.colour_range,
                    json_string(control::colour_range_name(s.colour_range)),
                    s.picture[0],
                    s.picture[1],
                    s.picture[2],
                    s.picture[3],
                    s.audio_gain,
                    control::gain_db(s.audio_gain)
                )),
                Err(e) => fields.push(format!("\"settings\":{{\"error\":{}}}", json_string(&e))),
            }
        }
        None => fields
            .push("\"device\":{\"present\":false,\"error\":\"no HD60 S attached\"}".to_string()),
    }
    if let Some(snapshot) = snapshot.as_ref() {
        match &snapshot.edid {
            Ok(block) => fields.push(format!(
                "\"edid\":{{\"summary\":{},\"valid\":{},\"factory\":{}}}",
                json_string(&edid::summary(block)),
                edid::validate(block).is_ok(),
                *block == edid::FACTORY
            )),
            Err(e) => fields.push(format!("\"edid\":{{\"error\":{}}}", json_string(e))),
        }
        let mcu = snapshot
            .mcu
            .iter()
            .map(|(command, meaning, reply)| {
                let reply = match reply {
                    Ok(r) => format!("{:02x} {:02x} {:02x}", r[0], r[1], r[2]),
                    Err(e) => e.clone(),
                };
                format!(
                    "{{\"command\":\"{command:#04x}\",\"meaning\":{},\"reply\":{}}}",
                    json_string(meaning),
                    json_string(&reply)
                )
            })
            .collect::<Vec<_>>();
        fields.push(format!("\"mcu\":[{}]", mcu.join(",")));
        fields.push(format!(
            "\"snapshot_age\":{}",
            snapshot.taken.elapsed().as_secs()
        ));
    }
    format!("{{{}}}", fields.join(","))
}

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
    let wants_picture = ["brightness", "contrast", "saturation", "hue"]
        .iter()
        .any(|key| request.number(key).is_some());
    if wants_picture {
        let mut picture = control.settings()?.picture;
        for (index, key) in ["brightness", "contrast", "saturation", "hue"]
            .iter()
            .enumerate()
        {
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
    Ok(format!(
        "{{\"ok\":true,\"changed\":{}}}",
        json_string(&changed.join(" "))
    ))
}

/// Converts the latest 1920x1080 YUYV frame into a 640x360 JPEG.
fn preview(shared: &Shared) -> Option<Vec<u8>> {
    let frame = shared.latest.lock().unwrap().clone()?;
    const SRC_W: usize = crate::frame::MAX_WIDTH;
    const SRC_H: usize = crate::frame::MAX_HEIGHT;
    const W: usize = SRC_W / 3;
    const H: usize = SRC_H / 3;
    if frame.len() != SRC_W * SRC_H * 2 {
        return None;
    }
    let mut rgb = vec![0_u8; W * H * 3];
    for y in 0..H {
        let row = &frame[y * 3 * SRC_W * 2..];
        for x in 0..W {
            let sx = x * 3;
            let pair = (sx / 2) * 4;
            let luma = row[pair + if sx % 2 == 0 { 0 } else { 2 }] as f32;
            let u = row[pair + 1] as f32 - 128.0;
            let v = row[pair + 3] as f32 - 128.0;
            // BT.709, limited range.
            let yy = (luma - 16.0) * 1.164;
            let r = yy + 1.793 * v;
            let g = yy - 0.213 * u - 0.533 * v;
            let b = yy + 2.112 * u;
            let at = (y * W + x) * 3;
            rgb[at] = r.clamp(0.0, 255.0) as u8;
            rgb[at + 1] = g.clamp(0.0, 255.0) as u8;
            rgb[at + 2] = b.clamp(0.0, 255.0) as u8;
        }
    }
    let mut out = Vec::new();
    let encoder = jpeg_encoder::Encoder::new(&mut out, 80);
    encoder
        .encode(&rgb, W as u16, H as u16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(out)
}

fn handle(mut stream: TcpStream, shared: Arc<Shared>) {
    let Some(request) = parse(&mut stream) else {
        return;
    };
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        ("GET", "/api/state") => respond(
            &mut stream,
            "200 OK",
            "application/json",
            state_json(&shared).as_bytes(),
        ),
        ("POST", "/api/set") => match apply(&shared, &request) {
            Ok(body) => respond(&mut stream, "200 OK", "application/json", body.as_bytes()),
            Err(error) => respond(
                &mut stream,
                "500 Internal Server Error",
                "application/json",
                format!("{{\"ok\":false,\"error\":{}}}", json_string(&error)).as_bytes(),
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
            match result {
                Ok(message) => respond(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    format!("{{\"ok\":true,\"message\":{}}}", json_string(&message)).as_bytes(),
                ),
                Err(error) => respond(
                    &mut stream,
                    "500 Internal Server Error",
                    "application/json",
                    format!("{{\"ok\":false,\"error\":{}}}", json_string(&error)).as_bytes(),
                ),
            }
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
        ("GET", "/preview.jpg") => match preview(&shared) {
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
