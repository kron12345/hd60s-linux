//! The control program for `hd60s-linux serve`: talks to the service over
//! its Unix socket, shows the live picture and everything the card
//! reports, and changes what can be changed. Nothing here touches USB.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use slint::{Image, Rgb8Pixel, SharedPixelBuffer};

slint::include_modules!();

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hd60s-linux/api.sock")
}

/// One HTTP request over the Unix socket; returns status and body.
fn api(method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let mut stream = UnixStream::connect(socket_path())
        .map_err(|error| format!("service not reachable ({error}); is `hd60s-serve.service` running?"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(format!("{method} {path} HTTP/1.1\r\nHost: local\r\nConnection: close\r\n\r\n").as_bytes())
        .map_err(|e| e.to_string())?;
    let mut data = Vec::new();
    stream.read_to_end(&mut data).map_err(|e| e.to_string())?;
    let split = data
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed reply")?;
    let head = String::from_utf8_lossy(&data[..split]);
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok((status, data[split + 4..].to_vec()))
}

fn state() -> Result<Value, String> {
    let (_, body) = api("GET", "/api/state")?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

fn post(path: &str) -> String {
    match api("POST", path) {
        Ok((_, body)) => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            v["message"]
                .as_str()
                .or(v["changed"].as_str())
                .or(v["error"].as_str())
                .unwrap_or("")
                .to_string()
        }
        Err(error) => error,
    }
}

/// BT.709 limited range, integer arithmetic.
fn yuyv_to_rgb(frame: &[u8]) -> SharedPixelBuffer<Rgb8Pixel> {
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(WIDTH as u32, HEIGHT as u32);
    let pixels = buffer.make_mut_slice();
    for (i, quad) in frame.as_chunks::<4>().0.iter().enumerate() {
        let (y0, u, y1, v) = (quad[0] as i32, quad[1] as i32 - 128, quad[2] as i32 - 128, quad[3] as i32 - 128);
        let r_add = (459 * v) >> 8;
        let g_add = -((55 * u + 136 * v) >> 8);
        let b_add = (541 * u) >> 8;
        for (k, y) in [y0, y1].into_iter().enumerate() {
            let luma = (298 * (y - 16)) >> 8;
            pixels[i * 2 + k] = Rgb8Pixel {
                r: (luma + r_add).clamp(0, 255) as u8,
                g: (luma + g_add).clamp(0, 255) as u8,
                b: (luma + b_add).clamp(0, 255) as u8,
            };
        }
    }
    buffer
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").to_string()
}

fn apply_state(ui: &MainWindow, s: &Value) {
    let device = &s["device"];
    let present = device["present"].as_bool().unwrap_or(false);
    ui.set_connected(true);
    ui.set_device_present(present);
    let timing = &s["timing"];
    let signal = timing["present"].as_bool().unwrap_or(false);
    ui.set_headline(if !present {
        "no card on the bus".into()
    } else if signal {
        format!("{} · streaming", text(device, "revision")).into()
    } else {
        format!("{} · no HDMI signal", text(device, "revision")).into()
    });
    ui.set_input_text(
        timing["text"]
            .as_str()
            .or(timing["error"].as_str())
            .unwrap_or("–")
            .into(),
    );
    let st = &s["stream"];
    ui.set_stream_text(
        format!(
            "{:.1} fps · {} frame(s) · {} bad · {} format change(s) · source {}x{}",
            st["fps"].as_f64().unwrap_or(0.0),
            st["frames"],
            st["bad"],
            st["format_changes"],
            st["geometry"][0],
            st["geometry"][1]
        )
        .into(),
    );
    if present {
        ui.set_device_text(
            format!(
                "{} ({}) · firmware {} · MCU build {} · USB {} bus {} address {}",
                text(device, "revision"),
                text(device, "product_id"),
                text(device, "firmware"),
                text(device, "mcu_build"),
                text(device, "speed"),
                device["bus"],
                device["address"]
            )
            .into(),
        );
        let settings = &s["settings"];
        if settings["picture"].is_array() && !ui.get_interacting() {
            let p = &settings["picture"];
            let get = |i: usize| p[i].as_i64().unwrap_or(128) as i32;
            ui.set_brightness(get(0));
            ui.set_contrast(get(1));
            ui.set_saturation(get(2));
            ui.set_hue(get(3));
            ui.set_gain(settings["gain"].as_i64().unwrap_or(128) as i32);
            ui.set_gain_db(format!("{:+.1} dB", settings["gain_db"].as_f64().unwrap_or(0.0)).into());
            ui.set_range_index(settings["range"].as_i64().unwrap_or(0).min(2) as i32);
        }
    } else {
        ui.set_device_text(text(device, "error").into());
    }
    let edid = &s["edid"];
    ui.set_edid_text(
        if edid.is_object() {
            format!(
                "{} — {}{}",
                text(edid, "summary"),
                if edid["valid"].as_bool().unwrap_or(false) { "valid" } else { "INVALID" },
                if edid["factory"].as_bool().unwrap_or(false) { ", power-on block" } else { ", custom" }
            )
        } else {
            String::new()
        }
        .into(),
    );
    if let Some(mcu) = s["mcu"].as_array() {
        ui.set_mcu_text(
            mcu.iter()
                .map(|m| format!("{} {}: {}", text(m, "command"), text(m, "meaning"), text(m, "reply")))
                .collect::<Vec<_>>()
                .join("\n")
                .into(),
        );
    }
    let rec = &s["recording"];
    ui.set_recording(rec.is_object());
    ui.set_recording_text(
        if rec.is_object() {
            let secs = rec["seconds"].as_f64().unwrap_or(0.0) as u64;
            format!(
                "{} · {}:{:02} · {} MB · {} dropped",
                text(rec, "path"),
                secs / 60,
                secs % 60,
                rec["bytes"].as_u64().unwrap_or(0) / 1_000_000,
                rec["dropped"]
            )
        } else {
            format!("off · files go to {} · encoder {}", text(s, "record_dir"), text(s, "encoder"))
        }
        .into(),
    );
    let net = &s["network_stream"];
    ui.set_stream_available(net.is_object());
    ui.set_stream_on(net["enabled"].as_bool().unwrap_or(false));
    ui.set_network_text(
        if net.is_object() {
            if net["enabled"].as_bool().unwrap_or(false) {
                format!("{} · {} client(s)", text(net, "url"), net["clients"])
            } else {
                format!("off (would be {})", text(net, "url"))
            }
        } else {
            "not configured".into()
        }
        .into(),
    );
    if let Some(message) = s["message"].as_str() {
        ui.set_message(message.into());
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let busy = Arc::new(AtomicBool::new(false));

    // Actions: each request on its own thread, then a state refresh.
    let act = |ui: &MainWindow, path: String| {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let message = post(&path);
            let refreshed = state().ok();
            let _ = weak.upgrade_in_event_loop(move |ui| {
                if let Some(s) = &refreshed {
                    apply_state(&ui, s);
                }
                ui.set_message(message.into());
            });
        });
    };
    {
        let ui_handle = ui.as_weak();
        ui.on_set_control(move |key, value| {
            let ui = ui_handle.unwrap();
            act(&ui, format!("/api/set?{key}={value}"));
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_set_range(move |index| act(&ui_handle.unwrap(), format!("/api/set?range={index}")));
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_set_gain(move |value| act(&ui_handle.unwrap(), format!("/api/set?gain={value}")));
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_reset_picture(move || act(&ui_handle.unwrap(), "/api/set?reset=1".into()));
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_toggle_record(move || {
            let ui = ui_handle.unwrap();
            let path = if ui.get_recording() { "/api/record?stop=1" } else { "/api/record?start=1" };
            act(&ui, path.into());
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_toggle_stream(move || {
            let ui = ui_handle.unwrap();
            let path = if ui.get_stream_on() { "/api/stream?off=1" } else { "/api/stream?on=1" };
            act(&ui, path.into());
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_restore_edid(move || act(&ui_handle.unwrap(), "/api/edid?restore=1".into()));
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_generate_report(move || {
            let weak = ui_handle.clone();
            std::thread::spawn(move || {
                let report = match api("GET", "/api/report") {
                    Ok((_, body)) => String::from_utf8_lossy(&body).into_owned(),
                    Err(error) => error,
                };
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_report_text(report.into()));
            });
        });
    }

    // State once a second.
    {
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            loop {
                let result = state();
                let done = weak
                    .upgrade_in_event_loop(move |ui| match &result {
                        Ok(s) => apply_state(&ui, s),
                        Err(error) => {
                            ui.set_connected(false);
                            ui.set_device_present(false);
                            ui.set_headline(error.clone().into());
                        }
                    })
                    .is_err();
                if done {
                    break;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // Live picture: the latest frame, converted here, about 30 times a second.
    {
        let weak = ui.as_weak();
        let busy = busy.clone();
        std::thread::spawn(move || {
            let mut last_len = 0;
            loop {
                let started = Instant::now();
                if !busy.load(Ordering::Relaxed)
                    && let Ok((200, frame)) = api("GET", "/frame.yuyv")
                    && frame.len() == WIDTH * HEIGHT * 2
                {
                    last_len = frame.len();
                    let rgb = yuyv_to_rgb(&frame);
                    busy.store(true, Ordering::Relaxed);
                    let busy_done = busy.clone();
                    if weak
                        .upgrade_in_event_loop(move |ui| {
                            ui.set_preview(Image::from_rgb8(rgb));
                            busy_done.store(false, Ordering::Relaxed);
                        })
                        .is_err()
                    {
                        break;
                    }
                } else if last_len == 0 {
                    std::thread::sleep(Duration::from_millis(500));
                }
                if let Some(rest) = Duration::from_millis(33).checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        });
    }

    ui.run()
}
