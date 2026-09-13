//! Recording from inside `serve`: the letterboxed frames and the audio are
//! handed to an `ffmpeg` child through bounded queues and written to a
//! Matroska file (H.264 + AAC). A slow disk or encoder costs frames, which
//! are counted, but never stalls the capture — the same rule as everywhere
//! else in this tool.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoder {
    /// VA-API when a quick test encode succeeds, libx264 otherwise.
    Auto,
    /// libx264, preset veryfast, CRF 20 — works everywhere.
    X264,
    /// H.264 through VA-API on /dev/dri/renderD128 (Intel/AMD).
    Vaapi,
}

impl Encoder {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "x264" | "software" => Some(Self::X264),
            "vaapi" => Some(Self::Vaapi),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::X264 => "x264",
            Self::Vaapi => "vaapi",
        }
    }

    /// Turns `Auto` into a concrete encoder by encoding one frame with
    /// VA-API (256x256: the encoders want at least 128 pixels a side); takes
    /// well under a second.
    pub fn resolve(self) -> Encoder {
        if self != Self::Auto {
            return self;
        }
        let ok = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-vaapi_device",
                VAAPI_DEVICE,
                "-f",
                "lavfi",
                "-i",
                "color=size=256x256:rate=1",
                "-frames:v",
                "1",
                "-vf",
                "format=nv12,hwupload",
                "-c:v",
                "h264_vaapi",
                "-f",
                "null",
                "-",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok { Self::Vaapi } else { Self::X264 }
    }
}

const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// What the panel and the tray show about a running or finished recording.
#[derive(Clone, Debug)]
pub struct Status {
    pub path: PathBuf,
    pub seconds: f64,
    pub bytes: u64,
    pub dropped_frames: u64,
}

pub struct Recording {
    path: PathBuf,
    started: Instant,
    pub encoder: Encoder,
    video: Option<SyncSender<Arc<Vec<u8>>>>,
    audio: Option<SyncSender<Vec<u8>>>,
    dropped: Arc<AtomicU64>,
    child: Child,
    fifo: PathBuf,
    threads: Vec<std::thread::JoinHandle<()>>,
}

/// Local time as `YYYY-MM-DD HH-MM-SS` for the file name.
fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as libc::time_t)
        .unwrap_or(0);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

impl Recording {
    /// Starts ffmpeg and the writer threads; frames are `width`x`height`
    /// YUYV, audio s16le stereo 48 kHz.
    pub fn start(
        dir: &Path,
        encoder: Encoder,
        width: usize,
        height: usize,
    ) -> Result<Self, String> {
        let encoder = encoder.resolve();
        std::fs::create_dir_all(dir)
            .map_err(|error| format!("creating {}: {error}", dir.display()))?;
        let path = dir.join(format!("HD60 S {}.mkv", timestamp()));
        let fifo = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(format!("hd60s-audio-{}.fifo", std::process::id()));
        let _ = std::fs::remove_file(&fifo);
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
            .map_err(|_| "fifo path".to_string())?;
        if unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) } != 0 {
            return Err(format!(
                "creating the audio fifo {}: {}",
                fifo.display(),
                std::io::Error::last_os_error()
            ));
        }

        let mut command = Command::new("ffmpeg");
        command
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .args([
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "yuyv422",
            ])
            .args([
                "-s",
                &format!("{width}x{height}"),
                "-framerate",
                "60",
                "-i",
                "pipe:0",
            ])
            .args(["-f", "s16le", "-ar", "48000", "-ac", "2", "-i"])
            .arg(&fifo);
        match encoder {
            Encoder::Auto | Encoder::X264 => {
                command.args([
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
                ]);
            }
            Encoder::Vaapi => {
                command.args([
                    "-vaapi_device",
                    VAAPI_DEVICE,
                    "-vf",
                    "format=nv12,hwupload",
                    "-c:v",
                    "h264_vaapi",
                    "-qp",
                    "22",
                ]);
            }
        }
        command
            .args([
                "-c:a",
                "aac",
                "-b:a",
                "192k",
                "-fps_mode",
                "cfr",
                "-r",
                "60",
            ])
            .arg(&path)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|error| {
            let _ = std::fs::remove_file(&fifo);
            format!("starting ffmpeg: {error} (is ffmpeg installed?)")
        })?;
        let mut stdin = child.stdin.take().ok_or("ffmpeg stdin")?;

        let dropped = Arc::new(AtomicU64::new(0));
        let (video_tx, video_rx) = sync_channel::<Arc<Vec<u8>>>(8);
        let (audio_tx, audio_rx) = sync_channel::<Vec<u8>>(4096);
        let video_thread = std::thread::spawn(move || {
            while let Ok(frame) = video_rx.recv() {
                if stdin.write_all(&frame).is_err() {
                    break;
                }
            }
            // Dropping stdin ends the video input.
        });
        let fifo_path = fifo.clone();
        let audio_thread = std::thread::spawn(move || {
            let Ok(mut out) = std::fs::OpenOptions::new().write(true).open(&fifo_path) else {
                return;
            };
            while let Ok(bytes) = audio_rx.recv() {
                if out.write_all(&bytes).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            path,
            encoder,
            started: Instant::now(),
            video: Some(video_tx),
            audio: Some(audio_tx),
            dropped,
            child,
            fifo,
            threads: vec![video_thread, audio_thread],
        })
    }

    pub fn push_frame(&self, pixels: Arc<Vec<u8>>) {
        if let Some(video) = &self.video {
            match video.try_send(pixels) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    pub fn push_audio(&self, bytes: &[u8]) {
        if let Some(audio) = &self.audio {
            let _ = audio.try_send(bytes.to_vec());
        }
    }

    pub fn status(&self) -> Status {
        Status {
            path: self.path.clone(),
            seconds: self.started.elapsed().as_secs_f64(),
            bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
            dropped_frames: self.dropped.load(Ordering::Relaxed),
        }
    }

    /// Whether ffmpeg is still running (it exits on an encoder error).
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ends both inputs, lets ffmpeg finish the file and returns the result.
    pub fn stop(mut self) -> Result<Status, String> {
        self.video.take();
        self.audio.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        let exit = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100))
                }
                _ => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    break None;
                }
            }
        };
        let _ = std::fs::remove_file(&self.fifo);
        let status = self.status();
        match exit {
            Some(code) if code.success() => Ok(status),
            Some(code) => Err(format!(
                "ffmpeg ended with {code}; file: {}",
                status.path.display()
            )),
            None => Err(format!(
                "ffmpeg did not finish in time and was killed; file: {}",
                status.path.display()
            )),
        }
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_file(&self.fifo);
    }
}
