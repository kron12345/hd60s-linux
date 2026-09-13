//! The program's tray icon: state colour, a short menu, and the way back
//! to the window when it was closed to the tray.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use ksni::blocking::TrayMethods;
use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip};
use serde_json::Value;

use crate::Runtime;

pub struct Tray {
    runtime: Arc<Runtime>,
    state: u8, // 0 no service/card, 1 no signal, 2 streaming
    description: String,
    recording: bool,
    muted: bool,
}

fn circle(size: i32, state: u8) -> Icon {
    let (r, g, b) = match state {
        0 => (0x8a, 0x8f, 0x98),
        1 => (0xe0, 0xa6, 0x40),
        _ => (0x5e, 0xc2, 0x7a),
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();
            let a = ((radius - d + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            data.extend_from_slice(&[a, r, g, b]);
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

impl Tray {
    pub fn apply(&mut self, state: Option<&Value>) {
        match state {
            None => {
                self.state = 0;
                self.description = "service not running".into();
                self.recording = false;
            }
            Some(s) => {
                let present = s["device"]["present"].as_bool().unwrap_or(false);
                let signal = s["timing"]["present"].as_bool().unwrap_or(false);
                self.state = match (present, signal) {
                    (false, _) => 0,
                    (true, false) => 1,
                    (true, true) => 2,
                };
                self.recording = s["recording"].is_object();
                self.muted = s["settings"]["gain"].as_i64() == Some(0);
                let t = &s["timing"];
                self.description = match self.state {
                    0 => "no card on the bus".into(),
                    1 => "card attached, no HDMI signal".into(),
                    _ => format!(
                        "{}x{} {} Hz · {:.1} fps{}",
                        t["width"],
                        t["height"],
                        t["refresh"],
                        s["stream"]["fps"].as_f64().unwrap_or(0.0),
                        if self.recording { " · ● REC" } else { "" }
                    ),
                };
            }
        }
    }

    fn show_window(&self) {
        let runtime = self.runtime.clone();
        let _ = slint::invoke_from_event_loop(move || crate::show_window(&runtime));
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "hd60s-control".into()
    }
    fn title(&self) -> String {
        "Elgato HD60 S".into()
    }
    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![
            circle(22, self.state),
            circle(32, self.state),
            circle(48, self.state),
        ]
    }
    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Elgato HD60 S".into(),
            description: self.description.clone(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.show_window();
    }
    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            MenuItem::Standard(StandardItem {
                label: self.description.clone(),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Show window".into(),
                activate: Box::new(|tray: &mut Self| tray.show_window()),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: if self.recording {
                    "Stop recording"
                } else {
                    "Start recording"
                }
                .into(),
                enabled: self.state == 2 || self.recording,
                activate: Box::new(|tray: &mut Self| {
                    let path = if tray.recording {
                        "/api/record?stop=1"
                    } else {
                        "/api/record?start=1"
                    };
                    let _ = crate::post(path);
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: if self.muted {
                    "Unmute audio"
                } else {
                    "Mute audio"
                }
                .into(),
                enabled: self.state != 0,
                activate: Box::new(|tray: &mut Self| {
                    let _ = crate::post(if tray.muted {
                        "/api/set?gain=128"
                    } else {
                        "/api/set?gain=0"
                    });
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    tray.runtime.quitting.store(true, Ordering::Relaxed);
                    let _ = slint::invoke_from_event_loop(|| {
                        let _ = slint::quit_event_loop();
                    });
                }),
                ..Default::default()
            }),
        ]
    }
}

/// Publishes the icon; returns the handle used to refresh it. `None` when
/// there is no session bus or no tray host.
pub fn start(runtime: Arc<Runtime>) -> Option<ksni::blocking::Handle<Tray>> {
    let tray = Tray {
        runtime,
        state: 0,
        description: "starting".into(),
        recording: false,
        muted: false,
    };
    match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(error) => {
            eprintln!("tray icon not shown: {error}");
            None
        }
    }
}
