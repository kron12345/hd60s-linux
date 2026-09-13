//! Publishes the capture as a PipeWire camera and a PipeWire audio source, so
//! OBS, browsers and every other PipeWire client see an ordinary camera and
//! microphone — no kernel module, no v4l2loopback, no root.
//!
//! Three threads: the pump (USB or file → frames and audio), the PipeWire
//! audio source on its own main loop, and the camera on the calling thread.
//! Video is handed over as the latest complete frame; a consumer that runs
//! slower than the source simply sees fewer frames, one that runs faster sees
//! repeats. Audio goes through a bounded ring buffer.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use pipewire as pw;
use pipewire_vircam::{Camera, Config, Format, Mode, State};
use pw::{properties::properties, spa};
use rusb::UsbContext;

use crate::frame::{self, letterbox};
use crate::pump::{self, Input};

const CANVAS_WIDTH: usize = frame::MAX_WIDTH;
const CANVAS_HEIGHT: usize = frame::MAX_HEIGHT;
const AUDIO_RATE: u32 = 48_000;
const AUDIO_CHANNELS: u32 = 2;
/// One second of audio; anything older is dropped rather than delaying the picture.
const AUDIO_RING_BYTES: usize = (AUDIO_RATE * AUDIO_CHANNELS * 2) as usize;

struct Shared {
    latest: Mutex<Option<Vec<u8>>>,
    audio: Mutex<VecDeque<u8>>,
    stop: AtomicBool,
}

/// Runs the camera and the audio source until the process is terminated.
pub fn serve<T: UsbContext + 'static>(name: &str, input: Input<T>) -> Result<(), String> {
    pw::init();
    let shared = Arc::new(Shared {
        latest: Mutex::new(None),
        audio: Mutex::new(VecDeque::with_capacity(AUDIO_RING_BYTES)),
        stop: AtomicBool::new(false),
    });

    // Pump: device or file → shared state.
    let pump_shared = shared.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let pump_stop = stop.clone();
    let pump_thread = std::thread::spawn(move || {
        let result = pump::run(
            input,
            pump_stop,
            None,
            |frame| {
                let pixels = letterbox(frame, CANVAS_WIDTH, CANVAS_HEIGHT);
                *pump_shared.latest.lock().unwrap() = Some(pixels);
            },
            |bytes| {
                let mut ring = pump_shared.audio.lock().unwrap();
                ring.extend(bytes);
                if ring.len() > AUDIO_RING_BYTES {
                    let excess = ring.len() - AUDIO_RING_BYTES;
                    ring.drain(..excess);
                }
            },
        );
        pump_shared.stop.store(true, Ordering::Relaxed);
        result
    });

    // Audio source on its own PipeWire main loop.
    let audio_shared = shared.clone();
    let audio_name = format!("{name} Audio");
    let audio_thread = std::thread::spawn(move || {
        let result = audio_source(&audio_name, audio_shared);
        if let Err(error) = &result {
            eprintln!("audio source: {error}");
        }
        result
    });

    // Camera on this thread; vircam owns the main loop.
    let modes = vec![Mode {
        width: CANVAS_WIDTH as u32,
        height: CANVAS_HEIGHT as u32,
        fps: vec![60, 50, 30],
        formats: vec![Format::Yuy2],
    }];
    let camera = Camera::new(Config {
        name: name.to_string(),
        media_name: name.to_string(),
        modes,
        max_buffers: 4,
    })
    .map_err(|error| format!("creating the PipeWire camera: {error}"))?;
    // The quit handle stays on this thread; the fill callback (which runs on
    // the camera's loop) ends the camera once the pump has stopped.
    let quit = camera.quit_handle();
    eprintln!("camera \"{name}\" and audio source \"{name} Audio\" published; Ctrl-C to stop");

    let fill_shared = shared.clone();
    let result = camera
        .on_state(|state| match state {
            State::Streaming { .. } => eprintln!("camera: a consumer is watching"),
            State::Paused { .. } => eprintln!("camera: idle"),
            State::Disconnected { error: Some(error) } => {
                eprintln!("camera: disconnected: {error}")
            }
            State::Disconnected { error: None } => {}
        })
        .run(move |frame, _negotiated| {
            if fill_shared.stop.load(Ordering::Relaxed) {
                frame.fill_black();
                quit.quit();
                return;
            }
            let latest = fill_shared.latest.lock().unwrap();
            match (latest.as_deref(), frame.format) {
                (Some(pixels), Format::Yuy2)
                    if pixels.len() == CANVAS_WIDTH * CANVAS_HEIGHT * 2 =>
                {
                    let plane = &mut frame.planes[0];
                    let stride = plane.stride as usize;
                    let rows = (plane.height as usize).min(CANVAS_HEIGHT);
                    let row_bytes = (CANVAS_WIDTH * 2).min(stride);
                    for row in 0..rows {
                        plane.data[row * stride..row * stride + row_bytes].copy_from_slice(
                            &pixels[row * CANVAS_WIDTH * 2..row * CANVAS_WIDTH * 2 + row_bytes],
                        );
                    }
                }
                _ => frame.fill_black(),
            }
        });

    stop.store(true, Ordering::Relaxed);
    shared.stop.store(true, Ordering::Relaxed);
    let pump_result = pump_thread
        .join()
        .unwrap_or_else(|_| Err("pump thread panicked".into()));
    match audio_thread.join() {
        Ok(Err(error)) => eprintln!("audio source: {error}"),
        Err(_) => eprintln!("audio source: thread panicked"),
        Ok(Ok(())) => {}
    }
    result.map_err(|error| format!("running the PipeWire camera: {error}"))?;
    let stats = pump_result?;
    eprintln!(
        "stopped: {} frame(s), {} bad, {} audio block(s), {} format change(s)",
        stats.frames, stats.bad_frames, stats.audio_blocks, stats.format_changes
    );
    Ok(())
}

/// A virtual PipeWire microphone fed from the ring buffer; silence when empty.
///
/// PipeWire creates the source node itself (`support.null-audio-sink` with
/// media class `Audio/Source/Virtual`, driven by its dummy clock) and this
/// process plays the captured audio into it as an ordinary playback stream —
/// the same arrangement virtual microphones in OBS and browsers use. A stream
/// node published directly as `Audio/Source` has no driver and is never
/// scheduled.
fn audio_source(name: &str, shared: Arc<Shared>) -> Result<(), String> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|error| error.to_string())?;
    let context =
        pw::context::ContextRc::new(&mainloop, None).map_err(|error| error.to_string())?;
    let core = context
        .connect_rc(None)
        .map_err(|error| error.to_string())?;

    // The virtual source lives as long as this proxy does.
    let _virtual_source = core
        .create_object::<pw::node::Node>(
            "adapter",
            &properties! {
                "factory.name" => "support.null-audio-sink",
                *pw::keys::NODE_NAME => name,
                *pw::keys::NODE_DESCRIPTION => name,
                *pw::keys::MEDIA_CLASS => "Audio/Source/Virtual",
                "audio.position" => "FL,FR",
                "audio.rate" => "48000",
                "audio.channels" => "2",
            },
        )
        .map_err(|error| format!("creating the virtual audio source: {error}"))?;
    // The session manager only ever links a playback stream to a sink and
    // otherwise falls back to the default output — the speakers. So the feed
    // stream is created unconnected and the two links into the virtual source
    // are made here by hand, port by port, once both nodes show in the registry.
    let registry = core.get_registry().map_err(|error| error.to_string())?;
    let objects: std::rc::Rc<std::cell::RefCell<Vec<RegistryEntry>>> = Default::default();
    let objects_clone = objects.clone();
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if let Some(props) = global.props {
                let get = |key: &str| props.get(key).map(str::to_owned);
                objects_clone.borrow_mut().push(RegistryEntry {
                    id: global.id,
                    type_: global.type_.to_str().to_owned(),
                    node_name: get("node.name"),
                    media_class: get("media.class"),
                    node_id: get("node.id").and_then(|v| v.parse().ok()),
                    direction: get("port.direction"),
                    channel: get("audio.channel"),
                });
            }
        })
        .register();
    let wait_for = |predicate: &dyn Fn(&RegistryEntry) -> bool| -> Option<u32> {
        for _ in 0..50 {
            roundtrip(&mainloop, &core);
            if let Some(entry) = objects.borrow().iter().find(|entry| predicate(entry)) {
                return Some(entry.id);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        None
    };
    let source_id = wait_for(&|entry| {
        entry.node_name.as_deref() == Some(name)
            && entry.media_class.as_deref() == Some("Audio/Source/Virtual")
    })
    .ok_or("the virtual audio source did not appear in the registry")?;

    let feed_name = format!("{name} feed");
    let stream = pw::stream::StreamBox::new(
        &core,
        &feed_name,
        properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Playback",
            *pw::keys::MEDIA_ROLE => "Production",
            *pw::keys::NODE_NAME => feed_name.as_str(),
            *pw::keys::NODE_AUTOCONNECT => "false",
            *pw::keys::AUDIO_CHANNELS => "2",
        },
    )
    .map_err(|error| error.to_string())?;

    let ring_shared = shared.clone();
    let counters = std::rc::Rc::new(std::cell::Cell::new((0_u64, 0_u64, 0_u64))); // calls, bytes, unmapped
    let counters_process = counters.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .process(move |stream, _| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let stride = (AUDIO_CHANNELS * 2) as usize;
            let data = &mut datas[0];
            let (mut calls, mut bytes, mut unmapped) = counters_process.get();
            calls += 1;
            let filled = if let Some(slice) = data.data() {
                let wanted = slice.len() / stride * stride;
                let mut ring = ring_shared.audio.lock().unwrap();
                let take = wanted.min(ring.len() / stride * stride);
                for (dst, src) in slice[..take].iter_mut().zip(ring.drain(..take)) {
                    *dst = src;
                }
                slice[take..wanted].fill(0);
                bytes += take as u64;
                wanted
            } else {
                unmapped += 1;
                0
            };
            counters_process.set((calls, bytes, unmapped));
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as _;
            *chunk.size_mut() = filled as _;
        })
        .register()
        .map_err(|error| error.to_string())?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(AUDIO_RATE);
    info.set_channels(AUDIO_CHANNELS);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object {
            type_: spa::sys::SPA_TYPE_OBJECT_Format,
            id: spa::sys::SPA_PARAM_EnumFormat,
            properties: info.into(),
        }),
    )
    .map_err(|error| format!("serializing the audio format: {error:?}"))?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).ok_or("building the audio format pod")?];
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut params,
        )
        .map_err(|error| error.to_string())?;

    // Link feed output ports to the virtual source's input ports, per channel.
    let feed_id = wait_for(&|entry| {
        entry.type_ == "PipeWire:Interface:Node"
            && entry.node_name.as_deref() == Some(feed_name.as_str())
    })
    .ok_or("the audio feed node did not appear in the registry")?;
    let mut links = Vec::new();
    for channel in ["FL", "FR"] {
        let output = wait_for(&|entry| {
            entry.type_ == "PipeWire:Interface:Port"
                && entry.node_id == Some(feed_id)
                && entry.direction.as_deref() == Some("out")
                && entry.channel.as_deref() == Some(channel)
        })
        .ok_or_else(|| format!("no output port {channel} on the audio feed"))?;
        let input = wait_for(&|entry| {
            entry.type_ == "PipeWire:Interface:Port"
                && entry.node_id == Some(source_id)
                && entry.direction.as_deref() == Some("in")
                && entry.channel.as_deref() == Some(channel)
        })
        .ok_or_else(|| format!("no input port {channel} on the virtual source"))?;
        let link = core
            .create_object::<pw::link::Link>(
                "link-factory",
                &properties! {
                    "link.output.node" => feed_id.to_string(),
                    "link.output.port" => output.to_string(),
                    "link.input.node" => source_id.to_string(),
                    "link.input.port" => input.to_string(),
                },
            )
            .map_err(|error| format!("linking channel {channel}: {error}"))?;
        links.push(link);
    }
    roundtrip(&mainloop, &core);

    // Leave the loop when the pump has ended.
    let loop_stop = mainloop.clone();
    let stop_shared = shared;
    let counters_timer = counters.clone();
    let ticks = std::cell::Cell::new(0_u32);
    let timer = mainloop.loop_().add_timer(move |_| {
        if stop_shared.stop.load(Ordering::Relaxed) {
            loop_stop.quit();
        }
        ticks.set(ticks.get() + 1);
        // A line every 30 s is enough to see that audio moves.
        if ticks.get().is_multiple_of(150) {
            let (calls, bytes, unmapped) = counters_timer.get();
            let queued = stop_shared.audio.lock().unwrap().len();
            eprintln!("audio: {calls} process call(s), {bytes} byte(s) delivered, {unmapped} unmapped, {queued} queued");
        }
    });
    timer
        .update_timer(
            Some(std::time::Duration::from_millis(200)),
            Some(std::time::Duration::from_millis(200)),
        )
        .into_result()
        .map_err(|error| error.to_string())?;
    mainloop.run();
    Ok(())
}

/// What the registry told us about an object; only the fields used here.
struct RegistryEntry {
    id: u32,
    type_: String,
    node_name: Option<String>,
    media_class: Option<String>,
    node_id: Option<u32>,
    direction: Option<String>,
    channel: Option<String>,
}

/// Waits until the core has processed everything sent so far.
fn roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let done = std::rc::Rc::new(std::cell::Cell::new(false));
    let done_clone = done.clone();
    let loop_clone = mainloop.clone();
    let Ok(pending) = core.sync(0) else {
        return;
    };
    let _listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_clone.set(true);
                loop_clone.quit();
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
}
