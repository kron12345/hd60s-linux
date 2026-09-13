//! An optional network stream for other machines and tools (Frigate,
//! go2rtc, a browser): Motion JPEG over HTTP, one encoder per client, off
//! by default and switched on from the panel or the tray. It is bound to
//! its own address because the panel stays on localhost; there is no
//! authentication, so keep it on a trusted network.

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::panel::{jpeg_from_yuyv, parse, respond};
use crate::serve::Shared;

const PAGE: &str = "<!doctype html><title>HD60 S stream</title><body style=\"margin:0;background:#000\"><img src=\"/stream.mjpg\" style=\"width:100%;height:100vh;object-fit:contain\">";

pub fn run(address: &str, shared: Arc<Shared>) -> Result<(), String> {
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("binding the network stream to {address}: {error}"))?;
    eprintln!(
        "network stream ready at http://{address}/stream.mjpg ({})",
        if shared.network_stream.load(Ordering::Relaxed) {
            "on"
        } else {
            "off until switched on"
        }
    );
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

fn handle(mut stream: TcpStream, shared: Arc<Shared>) {
    let Some(request) = parse(&mut stream) else {
        return;
    };
    if !shared.network_stream.load(Ordering::Relaxed) {
        respond(
            &mut stream,
            "503 Service Unavailable",
            "text/plain",
            b"the network stream is switched off",
        );
        return;
    }
    if let Some(token) = &shared.stream_token
        && request.get("token") != Some(token.as_str())
    {
        respond(
            &mut stream,
            "403 Forbidden",
            "text/plain",
            b"add ?token=... to the URL",
        );
        return;
    }
    let scale = request
        .number("scale")
        .map(usize::from)
        .unwrap_or(shared.stream_scale);
    let quality = request.number("quality").unwrap_or(80);
    match request.path.as_str() {
        "/" => respond(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            PAGE.as_bytes(),
        ),
        "/snapshot.jpg" => {
            let frame = shared.latest.lock().unwrap().clone();
            match frame.and_then(|f| jpeg_from_yuyv(&f, scale, quality)) {
                Some(jpeg) => respond(&mut stream, "200 OK", "image/jpeg", &jpeg),
                None => respond(&mut stream, "204 No Content", "text/plain", b""),
            }
        }
        "/stream.mjpg" => {
            let fps = request
                .number("fps")
                .map(u32::from)
                .unwrap_or(shared.stream_fps)
                .clamp(1, 60);
            shared.stream_clients.fetch_add(1, Ordering::Relaxed);
            mjpeg(&mut stream, &shared, scale, quality, fps);
            shared.stream_clients.fetch_sub(1, Ordering::Relaxed);
        }
        _ => respond(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn mjpeg(stream: &mut TcpStream, shared: &Shared, scale: usize, quality: u8, fps: u32) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }
    let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
    let mut last: Option<Arc<Vec<u8>>> = None;
    let mut next = Instant::now();
    while shared.network_stream.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        let frame = shared.latest.lock().unwrap().clone();
        let fresh = match (&frame, &last) {
            (Some(f), Some(l)) => !Arc::ptr_eq(f, l),
            (Some(_), None) => true,
            (None, _) => false,
        };
        if fresh && let Some(frame) = frame {
            if let Some(jpeg) = jpeg_from_yuyv(&frame, scale, quality) {
                let part = format!(
                    "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                    jpeg.len()
                );
                if stream.write_all(part.as_bytes()).is_err()
                    || stream.write_all(&jpeg).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            last = Some(frame);
        }
        next += interval;
        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        } else {
            next = Instant::now();
        }
    }
}
