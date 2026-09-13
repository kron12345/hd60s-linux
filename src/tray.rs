//! A tray icon (StatusNotifierItem over D-Bus, shown by Plasma, waybar and
//! other panels) for `serve`: the icon colour tells the state, the tooltip
//! the input, and the menu offers the everyday actions — open the control
//! panel, mute, reset the picture, colour range, quit.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ksni::blocking::TrayMethods;
use ksni::menu::{CheckmarkItem, MenuItem, RadioGroup, RadioItem, StandardItem};
use ksni::{Icon, ToolTip};

use crate::control;
use crate::serve::Shared;

/// What the icon shows, refreshed every two seconds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    NoCard,
    NoSignal,
    Streaming,
}

pub struct Tray {
    shared: Arc<Shared>,
    panel_url: Option<String>,
    state: State,
    description: String,
    muted: bool,
    range: usize,
    device_known: bool,
    recording: Option<crate::record::Status>,
    network_stream: bool,
}

impl Tray {
    fn refresh(&mut self) {
        let present = self.shared.device_present.load(Ordering::Relaxed);
        let control = self.shared.control.lock().unwrap();
        let (timing, settings) = match control.as_ref() {
            Some(control) => (control.timing().ok(), control.settings().ok()),
            None => (None, None),
        };
        drop(control);
        self.device_known = settings.is_some();
        if let Some(settings) = settings {
            self.muted = settings.audio_gain == 0;
            self.range = usize::from(settings.colour_range.min(2));
        }
        let fps = self.shared.recent_fps();
        let stats = *self.shared.stats.lock().unwrap();
        let geometry = *self.shared.geometry.lock().unwrap();
        self.recording = self.shared.recording_status();
        self.network_stream = self.shared.network_stream.load(Ordering::Relaxed);
        // Without a control connection (a replayed recording) the stream
        // geometry stands in for the timing registers.
        let signal = match &timing {
            Some(timing) => timing.present(),
            None => geometry.0 > 0,
        };
        self.state = match (present, signal) {
            (false, _) => State::NoCard,
            (true, true) => State::Streaming,
            (true, false) => State::NoSignal,
        };
        self.description = match self.state {
            State::NoCard => "no card on the bus".to_string(),
            State::NoSignal => "card attached, no HDMI signal".to_string(),
            State::Streaming => {
                let (width, height, refresh) = match &timing {
                    Some(t) => (
                        t.width as usize,
                        t.height as usize,
                        format!("{} Hz · ", t.refresh),
                    ),
                    None => (geometry.0, geometry.1, String::new()),
                };
                format!(
                    "{width}x{height} {refresh}{fps:.1} fps · {} frame(s), {} bad",
                    stats.frames, stats.bad_frames
                )
            }
        };
        if let Some(recording) = &self.recording {
            self.description = format!(
                "{} · ● REC {}:{:02}",
                self.description,
                recording.seconds as u64 / 60,
                recording.seconds as u64 % 60
            );
        }
    }

    fn open_panel(&self) {
        if let Some(url) = &self.panel_url {
            let _ = std::process::Command::new("xdg-open").arg(url).spawn();
        }
    }

    fn with_control(&self, action: impl FnOnce(&control::Control) -> Result<(), String>) {
        let control = self.shared.control.lock().unwrap();
        match control.as_ref() {
            Some(control) => {
                if let Err(error) = action(control) {
                    eprintln!("tray: {error}");
                }
            }
            None => eprintln!("tray: no card attached"),
        }
    }
}

/// A filled circle in the state colour, ARGB32 big-endian as the
/// specification wants it; no icon theme needed.
fn circle(size: i32, state: State) -> Icon {
    let (r, g, b) = match state {
        State::NoCard => (0x8a, 0x8f, 0x98),
        State::NoSignal => (0xe0, 0xa6, 0x40),
        State::Streaming => (0x5e, 0xc2, 0x7a),
    };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    let centre = (size as f32 - 1.0) / 2.0;
    let radius = size as f32 / 2.0 - 1.0;
    for y in 0..size {
        for x in 0..size {
            let d = ((x as f32 - centre).powi(2) + (y as f32 - centre).powi(2)).sqrt();
            let coverage = (radius - d + 0.5).clamp(0.0, 1.0);
            let a = (coverage * 255.0) as u8;
            data.extend_from_slice(&[a, r, g, b]);
        }
    }
    Icon {
        width: size,
        height: size,
        data,
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "hd60s-linux".into()
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
        self.open_panel();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![
            MenuItem::Standard(StandardItem {
                label: self.description.clone(),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
        ];
        if self.panel_url.is_some() {
            items.push(MenuItem::Standard(StandardItem {
                label: "Open control panel".into(),
                activate: Box::new(|tray: &mut Self| tray.open_panel()),
                ..Default::default()
            }));
        }
        items.extend([
            MenuItem::Standard(StandardItem {
                label: match &self.recording {
                    Some(r) => format!(
                        "Stop recording ({}:{:02})",
                        r.seconds as u64 / 60,
                        r.seconds as u64 % 60
                    ),
                    None => "Start recording".into(),
                },
                enabled: self.state == State::Streaming || self.recording.is_some(),
                activate: Box::new(|tray: &mut Self| {
                    let result = if tray.recording.is_some() {
                        tray.shared.stop_recording().map(|_| ())
                    } else {
                        tray.shared.start_recording().map(|_| ())
                    };
                    if let Err(error) = result {
                        eprintln!("tray: {error}");
                    }
                    tray.refresh();
                }),
                ..Default::default()
            }),
            MenuItem::Checkmark(CheckmarkItem {
                label: "Network stream (MJPEG)".into(),
                checked: self.network_stream,
                visible: self.shared.stream_bind.is_some(),
                activate: Box::new(|tray: &mut Self| {
                    let on = !tray.network_stream;
                    tray.shared.network_stream.store(on, Ordering::Relaxed);
                    eprintln!("network stream switched {}", if on { "on" } else { "off" });
                    tray.refresh();
                }),
                ..Default::default()
            }),
            MenuItem::Separator,
            MenuItem::Checkmark(CheckmarkItem {
                label: "Mute audio".into(),
                checked: self.muted,
                enabled: self.device_known,
                activate: Box::new(|tray: &mut Self| {
                    let gain = if tray.muted { 0x80 } else { 0x00 };
                    tray.with_control(|c| c.set_audio_gain(gain));
                    tray.refresh();
                }),
                ..Default::default()
            }),
            MenuItem::Standard(StandardItem {
                label: "Reset picture to neutral".into(),
                enabled: self.device_known,
                activate: Box::new(|tray: &mut Self| {
                    tray.with_control(|c| c.set_picture([0x80; 4]));
                }),
                ..Default::default()
            }),
            MenuItem::RadioGroup(RadioGroup {
                selected: self.range,
                select: Box::new(|tray: &mut Self, index: usize| {
                    tray.with_control(|c| c.set_colour_range(index as u8));
                    tray.range = index;
                }),
                options: vec![
                    RadioItem {
                        label: "Colour range: bypass".into(),
                        enabled: self.device_known,
                        ..Default::default()
                    },
                    RadioItem {
                        label: "Colour range: shrink".into(),
                        enabled: self.device_known,
                        ..Default::default()
                    },
                    RadioItem {
                        label: "Colour range: expand".into(),
                        enabled: self.device_known,
                        ..Default::default()
                    },
                ],
            }),
            MenuItem::Separator,
            MenuItem::Standard(StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| {
                    tray.shared.stop.store(true, Ordering::Relaxed);
                    std::process::exit(0);
                }),
                ..Default::default()
            }),
        ]);
        items
    }
}

/// Publishes the icon and keeps it current until the service stops. Without
/// a session bus or a status-notifier host this logs once and returns.
pub fn run(shared: Arc<Shared>, panel_url: Option<String>) {
    let mut tray = Tray {
        shared: shared.clone(),
        panel_url,
        state: State::NoCard,
        description: "starting".into(),
        muted: false,
        range: 0,
        device_known: false,
        recording: None,
        network_stream: false,
    };
    tray.refresh();
    let handle = match tray.spawn() {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("tray icon not shown: {error}");
            return;
        }
    };
    eprintln!("tray icon published");
    while !shared.stop.load(Ordering::Relaxed) && !handle.is_closed() {
        std::thread::sleep(Duration::from_secs(2));
        handle.update(|tray| tray.refresh());
    }
}
