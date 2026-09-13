//! `hd60s-linux`: the command line. Parses the arguments and hands over to
//! the module that does the work — `serve` for the service, `ctl` for
//! talking to it, `commands` for direct register access, `capture` for raw
//! frames, `research` for the protocol tools.

use std::process::ExitCode;

use hd60s_linux::commands::{self, EdidSettings, PictureSettings};
use hd60s_linux::record::Encoder;
use hd60s_linux::serve::{Options, Source, serve};
use hd60s_linux::{capture, config, ctl, device, report, research};
use rusb::Context;

const USAGE: &str = "usage: hd60s-linux COMMAND [OPTIONS]

  serve      run the service (PipeWire camera + microphone, API socket)
               [--name NAME] [--panel on|ADDR] [--record-dir DIR]
               [--record-encoder auto|x264|vaapi] [--stream on]
               [--stream-bind ADDR|off] [--stream-fps N] [--stream-scale N]
               [--stream-token T] [--from-file RAW [--fps N]]
               defaults come from ~/.config/hd60s-linux/serve.conf
  ctl        drive the running service: status|json|picture ...|range R|
               gain N|mute|unmute|reset|record start|stop|stream on|off|
               edid restore|dump FILE|snapshot FILE|report
  report     diagnostics for an issue [--show-serial] [--no-capture]

  Direct access to the card (not while the service runs):
  signal [SECONDS]            detected input timing, once or watched
  picture [--range bypass|shrink|expand] [--brightness N] [--contrast N]
          [--saturation N] [--hue N] [--reset]
  audio [--gain N|--mute]
  edid [--dump FILE] [--write FILE [--fix]] [--restore]
  mcu                         microcontroller status queries
  capture [SECONDS] [--audio FILE] [--native]   raw YUYV to stdout
  inspect [--show-serial]     USB descriptors
  observe|observe-stream|observe-iso [SECONDS]  protocol research";

/// The arguments after the command name.
struct Args(Vec<String>);

impl Args {
    fn value(&self, flag: &str) -> Option<&str> {
        self.0
            .iter()
            .position(|a| a == flag)
            .and_then(|at| self.0.get(at + 1))
            .map(String::as_str)
    }
    fn flag(&self, flag: &str) -> bool {
        self.0.iter().any(|a| a == flag)
    }
    fn number(&self, flag: &str) -> Result<Option<u8>, String> {
        self.value(flag)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("{flag} wants a number 0-255, not {v}"))
            })
            .transpose()
    }
    /// The first argument when it is not a flag, parsed as seconds.
    fn seconds(&self) -> Result<Option<u64>, String> {
        self.0
            .first()
            .filter(|a| !a.starts_with("--"))
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("{v} is not a number of seconds"))
            })
            .transpose()
    }
}

fn serve_command(args: &[String]) -> Result<(), String> {
    // Flags first, then the configuration file's defaults.
    let all = args
        .iter()
        .cloned()
        .chain(config::serve_config_arguments())
        .collect::<Vec<_>>();
    let args = Args(all);
    let name = args.value("--name").unwrap_or("Elgato HD60 S").to_string();
    let source = match args.value("--from-file") {
        Some(path) => Source::File {
            path: path.to_string(),
            fps: args
                .value("--fps")
                .and_then(|v| v.parse().ok())
                .unwrap_or(60.0),
        },
        None => Source::Usb,
    };
    // The web panel is off unless asked for: the control program and ctl
    // talk to the service over its Unix socket.
    let panel = match args.value("--panel") {
        None | Some("off") | Some("none") => None,
        Some("on") => Some("127.0.0.1:8060".to_string()),
        Some(address) => Some(address.to_string()),
    };
    let record_dir = match args.value("--record-dir") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => std::process::Command::new("xdg-user-dir")
            .arg("VIDEOS")
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join("Videos"))
            })
            .unwrap_or_else(|| std::path::PathBuf::from(".")),
    };
    let record_encoder = match args.value("--record-encoder") {
        Some(name) => Encoder::parse(name).ok_or("--record-encoder must be auto, x264 or vaapi")?,
        None => Encoder::Auto,
    };
    let options = Options {
        panel,
        record_dir,
        record_encoder,
        stream_bind: match args.value("--stream-bind") {
            Some("off") | Some("none") => None,
            Some(address) => Some(address.to_string()),
            None => Some("0.0.0.0:8061".to_string()),
        },
        stream_on: args.value("--stream") == Some("on"),
        stream_fps: args
            .value("--stream-fps")
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
        stream_scale: args
            .value("--stream-scale")
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        stream_token: args.value("--stream-token").map(str::to_string),
    };
    serve(&name, source, options)
}

fn picture_settings(args: &Args) -> Result<PictureSettings, String> {
    Ok(PictureSettings {
        range: match args.value("--range") {
            None => None,
            Some("bypass" | "standard") => Some(0),
            Some("shrink" | "expanded") => Some(1),
            Some("expand") => Some(2),
            Some(other) => Some(other.parse().map_err(|_| {
                format!("--range wants bypass, shrink, expand or 0-2, not {other}")
            })?),
        },
        controls: [
            args.number("--brightness")?,
            args.number("--contrast")?,
            args.number("--saturation")?,
            args.number("--hue")?,
        ],
        reset: args.flag("--reset"),
    })
}

/// Runs one command; `Err` becomes an error line and exit status 1.
fn run(command: &str, args: Args) -> Result<(), String> {
    let show_serial = args.flag("--show-serial");
    match command {
        "serve" => serve_command(&args.0),
        "ctl" => ctl::run(&args.0),
        "report" => {
            let seconds = if args.flag("--no-capture") { 0 } else { 3 };
            print!("{}", report::text(show_serial, seconds));
            Ok(())
        }
        "signal" => commands::signal(args.seconds()?),
        "picture" => commands::picture(picture_settings(&args)?),
        "audio" => commands::audio(if args.flag("--mute") {
            Some(0)
        } else {
            args.number("--gain")?
        }),
        "edid" => commands::edid(EdidSettings {
            dump: args.value("--dump").map(str::to_string),
            write: args.value("--write").map(str::to_string),
            restore: args.flag("--restore"),
            fix: args.flag("--fix"),
        }),
        "mcu" => commands::mcu(),
        // Everything below opens the card itself.
        _ => {
            let context =
                Context::new().map_err(|error| format!("initializing libusb: {error}"))?;
            let device = device::find(&context)
                .ok_or_else(|| format!("no HD60 S found; looked for {}", device::known_ids()))?;
            let seconds = args.seconds()?;
            match command {
                "capture" => capture::capture(
                    device,
                    seconds,
                    args.value("--audio"),
                    args.flag("--native"),
                ),
                "inspect" | "status" => {
                    let descriptor = device
                        .device_descriptor()
                        .map_err(|error| format!("reading the device descriptor: {error}"))?;
                    research::inspect_device(device, descriptor, show_serial)
                        .map_err(|error| format!("inspecting HD60 S: {error}"))
                }
                "observe" => research::observe_interrupt(device, seconds.unwrap_or(10))
                    .map_err(|error| format!("observing the interrupt endpoint: {error}")),
                "observe-stream" => research::observe_stream(device, seconds.unwrap_or(10)),
                "observe-iso" => research::observe_iso(&context, device, seconds.unwrap_or(10)),
                "startup-status" => research::read_direct_startup_status(device),
                _ => Err(USAGE.to_string()),
            }
        }
    }
}

fn main() -> ExitCode {
    // A closed pipe (`hd60s-linux capture | head -c ...`) ends the program
    // quietly instead of panicking on the next print.
    // SAFETY: resetting SIGPIPE to its default disposition has no other effect.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let mut arguments = std::env::args().skip(1);
    let Some(command) = arguments.next() else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    if command == "--help" || command == "-h" || command == "help" {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&command, Args(arguments.collect())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
