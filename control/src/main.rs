//! The control program for `hd60s-linux serve`: talks to the service over
//! its Unix socket, shows the live picture and everything the card
//! reports, and changes what can be changed. Nothing here touches USB
//! directly, except for checking whether the user may.
//!
//! The window is created when it is shown and dropped when it is closed
//! to the tray: a window that merely exists (even hidden) is listed by
//! the desktop as a running application.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use hd60s_api::{self as api, State};
use slint::Image;

slint::include_modules!();

mod preview;
mod service;
mod tray;
mod ui;

use service::{
    Access, OwnService, autostart_mode, device_access, read_config, start_own_service, systemctl,
    udev_rule_installed,
};
use ui::{WINDOW_OPEN, apply_service, apply_state, show_window, with_window};

const WIDTH: usize = 1920;
const HEIGHT: usize = 1080;
const UDEV_RULE: &str = include_str!("../../packaging/70-hd60s-linux.rules");
const UDEV_RULE_PATH: &str = "/etc/udev/rules.d/70-hd60s-linux.rules";

/// What the tray, the window and the threads share.
pub struct Runtime {
    pub quitting: AtomicBool,
    pub close_to_tray: AtomicBool,
    /// Preview downscaling factor, chosen from the widget's width.
    pub factor: AtomicUsize,
    /// Set while a converted frame waits for the UI thread.
    pub busy: AtomicBool,
    pub own: std::sync::Mutex<Option<OwnService>>,
    /// The latest state, for a window created later.
    pub last_state: std::sync::Mutex<Option<State>>,
    pub last_service: std::sync::Mutex<ServiceView>,
}

#[derive(Clone, Default)]
pub struct ServiceView {
    text: String,
    autostart: usize,
    running: bool,
    access_text: String,
    access_fixable: bool,
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
    let config = read_config();
    let runtime = Arc::new(Runtime {
        quitting: AtomicBool::new(false),
        close_to_tray: AtomicBool::new(
            config
                .get("close_to_tray")
                .map(|v| v != "false")
                .unwrap_or(true),
        ),
        factor: AtomicUsize::new(2),
        busy: AtomicBool::new(false),
        own: std::sync::Mutex::new(None),
        last_state: std::sync::Mutex::new(None),
        last_service: std::sync::Mutex::new(ServiceView::default()),
    });
    let tray_handle = tray::start(runtime.clone());
    if tray_handle.is_none() && start_in_tray {
        eprintln!("no tray available; showing the window instead");
    }

    // State once a second; service, autostart and device access every fifth time.
    {
        let runtime = runtime.clone();
        let tray_handle = tray_handle.clone();
        std::thread::spawn(move || {
            let mut tick = 0_u32;
            let mut tried_own = false;
            loop {
                let result = api::state();
                if result.is_ok() {
                    tried_own = false;
                }
                if result.is_err() && !tried_own && runtime.own.lock().unwrap().is_none() {
                    tried_own = true;
                    if systemctl(&["is-enabled"]).unwrap_or_default() != "enabled"
                        && let Ok(child) = start_own_service()
                    {
                        *runtime.own.lock().unwrap() = Some(child);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
                if let Some(handle) = &tray_handle {
                    let snapshot = result.as_ref().ok().cloned();
                    handle.update(|tray| tray.apply(snapshot.as_ref()));
                }
                if let Ok(s) = &result {
                    *runtime.last_state.lock().unwrap() = Some(s.clone());
                }
                if tick.is_multiple_of(5) {
                    let enabled = systemctl(&["is-enabled"]).unwrap_or_default();
                    let active = systemctl(&["is-active"]).unwrap_or_default() == "active";
                    let own_running = runtime.own.lock().unwrap().is_some();
                    let (access_text, access_fixable) = match device_access() {
                        Access::Denied(node) => (
                            format!("A card is plugged in but you may not open {node}: the udev access rule is missing."),
                            true,
                        ),
                        Access::NoCard if !udev_rule_installed() => (
                            "No udev access rule is installed yet; without it the card cannot be opened by your user.".to_string(),
                            true,
                        ),
                        _ => (String::new(), false),
                    };
                    let view = ServiceView {
                        text: match (active, own_running, result.is_ok()) {
                            (true, _, _) => "running as systemd user service".to_string(),
                            (false, true, true) => {
                                "running, started by this program (ends with it)".to_string()
                            }
                            (false, true, false) => "starting…".to_string(),
                            (false, false, true) => "running elsewhere".to_string(),
                            (false, false, false) => format!("not running (unit {enabled})"),
                        },
                        autostart: autostart_mode(),
                        running: result.is_ok(),
                        access_text,
                        access_fixable,
                    };
                    *runtime.last_service.lock().unwrap() = view.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        with_window(|ui| apply_service(ui, &view))
                    });
                }
                let _ = slint::invoke_from_event_loop(move || {
                    with_window(|ui| match &result {
                        Ok(s) => apply_state(ui, s),
                        Err(error) => {
                            ui.set_connected(false);
                            ui.set_device_present(false);
                            ui.set_headline(error.clone().into());
                        }
                    })
                });
                tick += 1;
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // Live picture: the latest frame, converted here, about 30 times a
    // second while a window exists.
    {
        let runtime = runtime.clone();
        std::thread::spawn(move || {
            loop {
                let started = Instant::now();
                let wanted = WINDOW_OPEN.load(Ordering::Relaxed);
                if wanted
                    && !runtime.busy.load(Ordering::Relaxed)
                    && let Ok((200, frame)) = api::request("GET", "/frame.yuyv")
                    && frame.len() == WIDTH * HEIGHT * 2
                {
                    let rgb =
                        preview::yuyv_to_rgb(&frame, runtime.factor.load(Ordering::Relaxed).max(1));
                    runtime.busy.store(true, Ordering::Relaxed);
                    let runtime_ui = runtime.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        with_window(|ui| {
                            ui.set_preview(Image::from_rgb8(rgb.clone()));
                            let factor = if ui.get_preview_width() >= 1500.0 {
                                1
                            } else {
                                2
                            };
                            runtime_ui.factor.store(factor, Ordering::Relaxed);
                        });
                        runtime_ui.busy.store(false, Ordering::Relaxed);
                    });
                } else if !wanted {
                    std::thread::sleep(Duration::from_millis(300));
                }
                if let Some(rest) = Duration::from_millis(33).checked_sub(started.elapsed()) {
                    std::thread::sleep(rest);
                }
            }
        });
    }

    if !start_in_tray || tray_handle.is_none() {
        show_window(&runtime);
    }
    let result = slint::run_event_loop_until_quit();
    // Our own service, if any, ends with the program.
    runtime.own.lock().unwrap().take();
    result
}
