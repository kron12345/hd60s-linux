//! The window: created when shown, dropped when closed to the tray, and
//! filled from the typed service state.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use hd60s_api::{self as api, Command, State};
use slint::ComponentHandle;

use crate::service::{
    install_udev_rule, set_autostart, start_own_service, systemctl, write_config,
};
use crate::{MainWindow, Runtime, ServiceView};

thread_local! {
    /// The window, when it exists; only the UI thread touches it.
    static WINDOW: RefCell<Option<MainWindow>> = const { RefCell::new(None) };
}

pub(crate) fn with_window(f: impl FnOnce(&MainWindow)) {
    WINDOW.with(|w| {
        if let Some(ui) = w.borrow().as_ref() {
            f(ui);
        }
    });
}

/// Whether a window exists, for the frame thread (it must not touch the
/// thread-local from another thread).
pub(crate) static WINDOW_OPEN: AtomicBool = AtomicBool::new(false);

pub(crate) fn apply_state(ui: &MainWindow, s: &State) {
    let device = &s.device;
    ui.set_connected(true);
    ui.set_device_present(device.present);
    let signal = s.timing.as_ref().is_some_and(|t| t.present);
    ui.set_headline(
        if !device.present {
            "no card on the bus".to_string()
        } else if signal {
            format!("{} · streaming", device.revision)
        } else {
            format!("{} · no HDMI signal", device.revision)
        }
        .into(),
    );
    ui.set_input_text(
        s.timing
            .as_ref()
            .map(|t| t.text.clone())
            .or_else(|| s.timing_error.clone())
            .unwrap_or_else(|| "–".into())
            .into(),
    );
    let st = &s.stream;
    ui.set_stream_text(
        format!(
            "{:.1} fps · {} frame(s) · {} bad · {} format change(s) · source {}x{}",
            st.fps, st.frames, st.bad, st.format_changes, st.geometry[0], st.geometry[1]
        )
        .into(),
    );
    if device.present {
        ui.set_device_text(
            format!(
                "{} ({}) · firmware {} · MCU build {} · USB {} bus {} address {}",
                device.revision,
                device.product_id,
                device.firmware,
                device.mcu_build,
                device.speed,
                device.bus,
                device.address
            )
            .into(),
        );
        if let Some(p) = &s.settings
            && !ui.get_interacting()
        {
            ui.set_brightness(p.picture[0] as i32);
            ui.set_contrast(p.picture[1] as i32);
            ui.set_saturation(p.picture[2] as i32);
            ui.set_hue(p.picture[3] as i32);
            ui.set_gain(p.gain as i32);
            ui.set_gain_db(format!("{:+.1} dB", p.gain_db).into());
            ui.set_range_index(p.range.min(2) as i32);
        }
    } else {
        ui.set_device_text(device.error.clone().unwrap_or_default().into());
    }
    ui.set_edid_text(
        match (&s.edid, &s.edid_error) {
            (Some(e), _) => format!(
                "{} — {}{}",
                e.summary,
                if e.valid { "valid" } else { "INVALID" },
                if e.factory {
                    ", power-on block"
                } else {
                    ", custom"
                }
            ),
            (None, Some(error)) => error.clone(),
            (None, None) => String::new(),
        }
        .into(),
    );
    ui.set_mcu_text(
        s.mcu
            .iter()
            .map(|m| format!("{} {}: {}", m.command, m.meaning, m.reply))
            .collect::<Vec<_>>()
            .join("\n")
            .into(),
    );
    ui.set_recording(s.recording.is_some());
    ui.set_recording_text(
        match &s.recording {
            Some(r) => {
                let secs = r.seconds as u64;
                format!(
                    "{} · {}:{:02} · {} MB · {} dropped",
                    r.path,
                    secs / 60,
                    secs % 60,
                    r.bytes / 1_000_000,
                    r.dropped
                )
            }
            None => format!("off · files go to {} · encoder {}", s.record_dir, s.encoder),
        }
        .into(),
    );
    ui.set_stream_available(s.network_stream.is_some());
    ui.set_stream_on(s.network_stream.as_ref().is_some_and(|n| n.enabled));
    ui.set_network_text(
        match &s.network_stream {
            Some(n) if n.enabled => format!("{} · {} client(s)", n.url, n.clients),
            Some(n) => format!("off (would be {})", n.url),
            None => "not configured".into(),
        }
        .into(),
    );
    if let Some(message) = &s.message {
        ui.set_message(message.clone().into());
    }
}

pub(crate) fn apply_service(ui: &MainWindow, view: &ServiceView) {
    ui.set_service_text(view.text.clone().into());
    ui.set_autostart_index(view.autostart as i32);
    ui.set_autostart_hint(
        match view.autostart {
            1 => "sway users: add `exec hd60s-control --tray` to the sway config instead.",
            2 => "The unit keeps running when this program is closed.",
            _ => "",
        }
        .into(),
    );
    ui.set_service_running(view.running);
    ui.set_access_text(view.access_text.clone().into());
    ui.set_access_fixable(view.access_fixable);
}

/// Sends a command off the UI thread, then refreshes.
pub(crate) fn act(command: Command) {
    std::thread::spawn(move || {
        let message = api::send(&command).text();
        let refreshed = api::state().ok();
        let _ = slint::invoke_from_event_loop(move || {
            with_window(|ui| {
                if let Some(s) = &refreshed {
                    apply_state(ui, s);
                }
                ui.set_message(message.clone().into());
            });
        });
    });
}

pub(crate) fn picture_key(key: &str) -> Option<&'static str> {
    ["brightness", "contrast", "saturation", "hue"]
        .into_iter()
        .find(|k| *k == key)
}

/// Creates the window with all its callbacks; the caller shows it.
pub(crate) fn build_window(runtime: &Arc<Runtime>) -> Result<MainWindow, slint::PlatformError> {
    let ui = MainWindow::new()?;
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
                // Drop the window entirely (after this callback returns).
                let _ = slint::invoke_from_event_loop(|| {
                    WINDOW.with(|w| {
                        if let Some(ui) = w.borrow_mut().take() {
                            let _ = ui.hide();
                        }
                    });
                    WINDOW_OPEN.store(false, Ordering::Relaxed);
                });
                slint::CloseRequestResponse::HideWindow
            } else {
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
    ui.on_set_control(|key, value| {
        if let Some(key) = picture_key(&key) {
            act(Command::Picture {
                key,
                value: value.clamp(0, 255) as u8,
            });
        }
    });
    ui.on_set_range(|index| act(Command::Range(index.clamp(0, 2) as u8)));
    ui.on_set_gain(|value| act(Command::Gain(value.clamp(0, 255) as u8)));
    ui.on_reset_picture(|| act(Command::ResetPicture));
    {
        let weak = ui.as_weak();
        ui.on_toggle_record(move || {
            let ui = weak.unwrap();
            act(if ui.get_recording() {
                Command::RecordStop
            } else {
                Command::RecordStart
            });
        });
    }
    {
        let weak = ui.as_weak();
        ui.on_toggle_stream(move || {
            let ui = weak.unwrap();
            act(if ui.get_stream_on() {
                Command::StreamOff
            } else {
                Command::StreamOn
            });
        });
    }
    ui.on_restore_edid(|| act(Command::RestoreEdid));
    ui.on_generate_report(|| {
        std::thread::spawn(|| {
            let report = match api::request("GET", "/api/report") {
                Ok((_, body)) => String::from_utf8_lossy(&body).into_owned(),
                Err(error) => error,
            };
            let _ = slint::invoke_from_event_loop(move || {
                with_window(|ui| ui.set_report_text(report.clone().into()))
            });
        });
    });
    {
        let runtime = runtime.clone();
        ui.on_set_autostart(move |index| {
            let runtime = runtime.clone();
            std::thread::spawn(move || {
                if index == 2 {
                    runtime.own.lock().unwrap().take();
                }
                let message = set_autostart(index as usize).unwrap_or_else(|e| e);
                let _ = slint::invoke_from_event_loop(move || {
                    with_window(|ui| ui.set_message(message.clone().into()))
                });
            });
        });
    }
    {
        let runtime = runtime.clone();
        ui.on_toggle_service(move || {
            let runtime = runtime.clone();
            std::thread::spawn(move || {
                let running = api::state().is_ok();
                let message = if running {
                    if runtime.own.lock().unwrap().take().is_some() {
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
                            *runtime.own.lock().unwrap() = Some(child);
                            "service started by this program".to_string()
                        }
                        Err(error) => error,
                    }
                };
                let _ = slint::invoke_from_event_loop(move || {
                    with_window(|ui| ui.set_message(message.clone().into()))
                });
            });
        });
    }
    ui.on_install_udev_rule(|| {
        std::thread::spawn(|| {
            let message = install_udev_rule().unwrap_or_else(|e| e);
            let _ = slint::invoke_from_event_loop(move || {
                with_window(|ui| ui.set_message(message.clone().into()))
            });
        });
    });
    // What the threads have learnt so far.
    if let Some(s) = runtime.last_state.lock().unwrap().as_ref() {
        apply_state(&ui, s);
    }
    apply_service(&ui, &runtime.last_service.lock().unwrap());
    Ok(ui)
}

/// Shows the window, creating it if needed (UI thread only).
pub(crate) fn show_window(runtime: &Arc<Runtime>) {
    WINDOW.with(|w| {
        let mut slot = w.borrow_mut();
        if slot.is_none() {
            match build_window(runtime) {
                Ok(ui) => *slot = Some(ui),
                Err(error) => {
                    eprintln!("creating the window: {error}");
                    return;
                }
            }
        }
        if let Some(ui) = slot.as_ref() {
            let _ = ui.show();
            ui.window().set_minimized(false);
            WINDOW_OPEN.store(true, Ordering::Relaxed);
        }
    });
}
