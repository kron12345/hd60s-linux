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

mod tray;

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;

/// What the tray and the window share.
pub struct Runtime {
    pub quitting: AtomicBool,
    pub close_to_tray: AtomicBool,
}

fn config_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("hd60s-linux/control.conf")
}

/// `key=value` lines; only `close_to_tray` so far.
fn read_config() -> std::collections::HashMap<String, String> {
    std::fs::read_to_string(config_path())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect()
}

fn write_config(key: &str, value: &str) {
    let mut config = read_config();
    config.insert(key.to_string(), value.to_string());
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = config
        .iter()
        .map(|(k, v)| format!("{k}={v}\n"))
        .collect::<String>();
    let _ = std::fs::write(path, text);
}

fn autostart_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("autostart/hd60s-control.desktop")
}

/// 0 none, 1 with the desktop (XDG autostart), 2 systemd user service.
fn autostart_mode() -> usize {
    if autostart_file().exists() {
        1
    } else if systemctl(&["is-enabled"]).as_deref() == Ok("enabled") {
        2
    } else {
        0
    }
}

fn set_autostart(mode: usize) -> Result<String, String> {
    let file = autostart_file();
    match mode {
        1 => {
            let _ = systemctl(&["disable", "--now"]);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
            }
            std::fs::write(
                &file,
                "[Desktop Entry]\nType=Application\nName=HD60 S Control\nExec=hd60s-control --tray\nIcon=hd60s-control\nX-GNOME-Autostart-enabled=true\n",
            )
            .map_err(|e| e.to_string())?;
            Ok("starts with the desktop, minimised to the tray (sway: add `exec hd60s-control --tray` to your config)".into())
        }
        2 => {
            let _ = std::fs::remove_file(&file);
            systemctl(&["unmask"])?;
            systemctl(&["enable", "--now"])?;
            Ok("systemd user service enabled: runs at login and with the card, even without this program".into())
        }
        _ => {
            let _ = std::fs::remove_file(&file);
            let _ = systemctl(&["disable", "--now"]);
            Ok("no autostart: the service runs while this program is open".into())
        }
    }
}

fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("hd60s-linux/api.sock")
}

/// One HTTP request over the Unix socket; returns status and body.
fn api(method: &str, path: &str) -> Result<(u16, Vec<u8>), String> {
    let mut stream = UnixStream::connect(socket_path()).map_err(|error| {
        format!("service not reachable ({error}); is `hd60s-serve.service` running?")
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
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

/// BT.709 limited range, integer arithmetic; `factor` > 1 averages
/// factor x factor source pixels (a box filter), which is what keeps the
/// scaled picture free of moiré.
fn yuyv_to_rgb(frame: &[u8], factor: usize) -> SharedPixelBuffer<Rgb8Pixel> {
    let (w, h) = (WIDTH / factor, HEIGHT / factor);
    let mut buffer = SharedPixelBuffer::<Rgb8Pixel>::new(w as u32, h as u32);
    let pixels = buffer.make_mut_slice();
    let samples = (factor * factor) as i32;
    for y in 0..h {
        for x in 0..w {
            let (mut sy, mut su, mut sv) = (0_i32, 0_i32, 0_i32);
            for dy in 0..factor {
                let row = &frame[(y * factor + dy) * WIDTH * 2..];
                for dx in 0..factor {
                    let sx = x * factor + dx;
                    let pair = (sx / 2) * 4;
                    sy += row[pair + if sx.is_multiple_of(2) { 0 } else { 2 }] as i32;
                    su += row[pair + 1] as i32;
                    sv += row[pair + 3] as i32;
                }
            }
            let (yv, u, v) = (sy / samples, su / samples - 128, sv / samples - 128);
            let luma = (298 * (yv - 16)) >> 8;
            pixels[y * w + x] = Rgb8Pixel {
                r: (luma + ((459 * v) >> 8)).clamp(0, 255) as u8,
                g: (luma - ((55 * u + 136 * v) >> 8)).clamp(0, 255) as u8,
                b: (luma + ((541 * u) >> 8)).clamp(0, 255) as u8,
            };
        }
    }
    buffer
}

/// `systemctl --user` for the service.
fn systemctl(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .arg("hd60s-serve.service")
        .output()
        .map_err(|e| format!("systemctl: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() || args[0].starts_with("is-") {
        Ok(text)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// The service run by this program when systemd is not running it.
struct OwnService(std::process::Child);

impl Drop for OwnService {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_own_service() -> Result<OwnService, String> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new("hd60s-linux");
    command
        .args(["serve", "--tray", "off"])
        .stdin(std::process::Stdio::null());
    // SAFETY: prctl only marks the child to receive SIGTERM when this
    // process dies, however it dies — the service must never outlive the
    // program that started it.
    unsafe {
        command.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    command
        .spawn()
        .map(OwnService)
        .map_err(|e| format!("starting hd60s-linux serve: {e}"))
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
            ui.set_gain_db(
                format!("{:+.1} dB", settings["gain_db"].as_f64().unwrap_or(0.0)).into(),
            );
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
                if edid["valid"].as_bool().unwrap_or(false) {
                    "valid"
                } else {
                    "INVALID"
                },
                if edid["factory"].as_bool().unwrap_or(false) {
                    ", power-on block"
                } else {
                    ", custom"
                }
            )
        } else {
            String::new()
        }
        .into(),
    );
    if let Some(mcu) = s["mcu"].as_array() {
        ui.set_mcu_text(
            mcu.iter()
                .map(|m| {
                    format!(
                        "{} {}: {}",
                        text(m, "command"),
                        text(m, "meaning"),
                        text(m, "reply")
                    )
                })
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
            format!(
                "off · files go to {} · encoder {}",
                text(s, "record_dir"),
                text(s, "encoder")
            )
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
    let start_in_tray = std::env::args().any(|a| a == "--tray");
    // The Wayland app id (and X11 class) must match the desktop entry for
    // the panel to show our icon and group the window.
    slint::BackendSelector::new()
        .with_winit_window_attributes_hook(|attributes| {
            use slint::winit_030::winit::platform::wayland::WindowAttributesExtWayland as W;
            use slint::winit_030::winit::platform::x11::WindowAttributesExtX11 as X;
            let attributes = W::with_name(attributes, "hd60s-control", "hd60s-control");
            X::with_name(attributes, "hd60s-control", "hd60s-control")
        })
        .select()?;
    let ui = MainWindow::new()?;
    let config = read_config();
    let runtime = Arc::new(Runtime {
        quitting: AtomicBool::new(false),
        close_to_tray: AtomicBool::new(
            config
                .get("close_to_tray")
                .map(|v| v != "false")
                .unwrap_or(true),
        ),
    });
    ui.set_close_to_tray(runtime.close_to_tray.load(Ordering::Relaxed));
    {
        let runtime = runtime.clone();
        ui.on_set_close_to_tray(move |on| {
            runtime.close_to_tray.store(on, Ordering::Relaxed);
            write_config("close_to_tray", if on { "true" } else { "false" });
        });
    }
    {
        let runtime = runtime.clone();
        ui.window().on_close_requested(move || {
            if runtime.close_to_tray.load(Ordering::Relaxed)
                && !runtime.quitting.load(Ordering::Relaxed)
            {
                slint::CloseRequestResponse::HideWindow
            } else {
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
    let tray_handle = tray::start(ui.as_weak(), runtime.clone());
    let busy = Arc::new(AtomicBool::new(false));
    // Downscaling factor for the preview, chosen from the widget's width.
    let factor = Arc::new(std::sync::atomic::AtomicUsize::new(2));
    let own: Arc<std::sync::Mutex<Option<OwnService>>> = Arc::new(std::sync::Mutex::new(None));

    // Service control: autostart mode, and the systemd unit or our own child.
    {
        let own = own.clone();
        let weak = ui.as_weak();
        ui.on_set_autostart(move |index| {
            let own = own.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                if index == 2 {
                    own.lock().unwrap().take();
                }
                let message = set_autostart(index as usize).unwrap_or_else(|e| e);
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_message(message.into()));
            });
        });
    }
    {
        let own = own.clone();
        let weak = ui.as_weak();
        ui.on_toggle_service(move || {
            let own = own.clone();
            let weak = weak.clone();
            std::thread::spawn(move || {
                let running = state().is_ok();
                let message = if running {
                    if own.lock().unwrap().take().is_some() {
                        "service stopped".to_string()
                    } else {
                        systemctl(&["stop"])
                            .map(|_| "service stopped".to_string())
                            .unwrap_or_else(|e| e)
                    }
                } else if systemctl(&["is-enabled"]).as_deref() == Ok("enabled") {
                    systemctl(&["start"])
                        .map(|_| "service started".to_string())
                        .unwrap_or_else(|e| e)
                } else {
                    match start_own_service() {
                        Ok(child) => {
                            *own.lock().unwrap() = Some(child);
                            "service started by this program".to_string()
                        }
                        Err(error) => error,
                    }
                };
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_message(message.into()));
            });
        });
    }

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
            let path = if ui.get_recording() {
                "/api/record?stop=1"
            } else {
                "/api/record?start=1"
            };
            act(&ui, path.into());
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_toggle_stream(move || {
            let ui = ui_handle.unwrap();
            let path = if ui.get_stream_on() {
                "/api/stream?off=1"
            } else {
                "/api/stream?on=1"
            };
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

    // State once a second; the service's systemd status every fifth time.
    {
        let weak = ui.as_weak();
        let own = own.clone();
        let tray_handle = tray_handle.clone();
        std::thread::spawn(move || {
            let mut tick = 0_u32;
            let mut tried_own = false;
            loop {
                let result = state();
                if result.is_ok() {
                    // Should the service go away later (the unit disabled
                    // from the Service box, say), run our own again.
                    tried_own = false;
                }
                if result.is_err() && !tried_own && own.lock().unwrap().is_none() {
                    // Nothing answers: unless systemd is meant to run it, run it ourselves.
                    tried_own = true;
                    let enabled = systemctl(&["is-enabled"]).unwrap_or_default();
                    if enabled != "enabled"
                        && let Ok(child) = start_own_service()
                    {
                        *own.lock().unwrap() = Some(child);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
                if let Some(handle) = &tray_handle {
                    let snapshot = result.as_ref().ok().cloned();
                    handle.update(|tray| tray.apply(snapshot.as_ref()));
                }
                if tick.is_multiple_of(5) {
                    let enabled = systemctl(&["is-enabled"]).unwrap_or_default();
                    let mode = autostart_mode();
                    let active = systemctl(&["is-active"]).unwrap_or_default() == "active";
                    let own_running = own.lock().unwrap().is_some();
                    let text = match (active, own_running, result.is_ok()) {
                        (true, _, _) => "running as systemd user service".to_string(),
                        (false, true, true) => {
                            "running, started by this program (ends with it)".to_string()
                        }
                        (false, true, false) => "starting…".to_string(),
                        (false, false, true) => "running elsewhere".to_string(),
                        (false, false, false) => format!("not running (unit {enabled})"),
                    };
                    let running = result.is_ok();
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_service_text(text.into());
                        ui.set_autostart_index(mode as i32);
                        ui.set_autostart_hint(
                            match mode {
                                1 => "sway users: add `exec hd60s-control --tray` to the sway config instead.",
                                2 => "The unit keeps running when this program is closed.",
                                _ => "",
                            }
                            .into(),
                        );
                        ui.set_service_running(running);
                    });
                }
                tick += 1;
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
        let factor = factor.clone();
        std::thread::spawn(move || {
            let mut last_len = 0;
            loop {
                let started = Instant::now();
                if !busy.load(Ordering::Relaxed)
                    && let Ok((200, frame)) = api("GET", "/frame.yuyv")
                    && frame.len() == WIDTH * HEIGHT * 2
                {
                    last_len = frame.len();
                    let rgb = yuyv_to_rgb(&frame, factor.load(Ordering::Relaxed).max(1));
                    busy.store(true, Ordering::Relaxed);
                    let busy_done = busy.clone();
                    let factor_out = factor.clone();
                    if weak
                        .upgrade_in_event_loop(move |ui| {
                            ui.set_preview(Image::from_rgb8(rgb));
                            // Full size only when the widget can show it.
                            let wanted = if ui.get_preview_width() >= 1500.0 {
                                1
                            } else {
                                2
                            };
                            factor_out.store(wanted, Ordering::Relaxed);
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

    if !start_in_tray {
        ui.show()?;
    }
    let result = slint::run_event_loop_until_quit();
    // Our own service, if any, ends with the program.
    own.lock().unwrap().take();
    result
}
