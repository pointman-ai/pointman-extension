//! A video pane from the Rust SDK: it opens with its size and rate, sends frames only while the node
//! says a path is attached, and follows the app's hints (`max_fps: 0` pauses it). Its frames stand in
//! for a camera's JPEGs: a JPEG's first and last markers around a count.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pointman_extension::{Extension, Pane, Refused, VIDEO_FRAME_MAX};
use serde_json::{json, Value};

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;

    ext.pane("live", |pane: &Pane| {
        if pane.target()["bad"] == true {
            return Ok(json!({"title": "Test card"})); // no video: the open fails
        }
        let size = if pane.target()["big"] == true { VIDEO_FRAME_MAX } else { 16 };
        // before the open is answered there's nobody to send it to: dropped
        let early = pane.frame(&frame(0, 16), 64, 48, now_ms());
        pane.handle().event("video.early", json!({"sent": early}));

        let fps = Arc::new(AtomicU64::new(30));
        let running: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::default();

        let (rate, feed) = (fps.clone(), running.clone());
        pane.on_viewers(move |pane: &Pane, attached: u64| {
            let mut feed = feed.lock().unwrap();
            if attached == 0 {
                if let Some(stop) = feed.take() {
                    stop.store(true, Ordering::SeqCst);
                }
            } else if feed.is_none() {
                let stop = Arc::new(AtomicBool::new(false));
                *feed = Some(stop.clone());
                let (pane, rate) = (pane.clone(), rate.clone());
                thread::spawn(move || send(&pane, &rate, &stop, size));
            }
            pane.handle().event("video.watching", json!({"attached": attached, "viewers": pane.viewers()}));
        });
        pane.on_input(move |_: &Pane, event: &Value| -> anyhow::Result<()> {
            if event["kind"] != "hint" {
                return Err(Refused::new("a video pane takes hints only").into());
            }
            fps.store(event["max_fps"].as_u64().unwrap_or(30).min(30), Ordering::SeqCst);
            Ok(())
        });
        pane.on_close(move |pane: &Pane, reason: &str| {
            if let Some(stop) = running.lock().unwrap().take() {
                stop.store(true, Ordering::SeqCst);
            }
            pane.handle().event("video.closed", json!({"pane": pane.id(), "reason": reason}));
        });
        Ok(json!({"title": "Test card", "video": {"codec": "mjpeg", "width": 64, "height": 48, "fps": 30}}))
    });

    ext.run()
}

/// Frames at the hinted rate until stopped; none while it's 0.
fn send(pane: &Pane, fps: &AtomicU64, stop: &AtomicBool, size: usize) {
    let h = pane.handle();
    h.event("video.capturing", json!({"on": true}));
    let mut n = 1;
    while !stop.load(Ordering::SeqCst) {
        let rate = fps.load(Ordering::SeqCst);
        if rate == 0 {
            thread::sleep(Duration::from_millis(20));
            continue;
        }
        pane.frame(&frame(n, size), 64, 48, now_ms());
        n += 1;
        thread::sleep(Duration::from_millis(1000 / rate));
    }
    h.event("video.capturing", json!({"on": false}));
}

/// `size` bytes from a JPEG's start marker to its end marker, with the count after the start.
fn frame(n: u64, size: usize) -> Vec<u8> {
    let mut jpeg = vec![0u8; size.max(12)];
    jpeg[..2].copy_from_slice(&[0xff, 0xd8]);
    jpeg[2..10].copy_from_slice(&n.to_be_bytes());
    let end = jpeg.len();
    jpeg[end - 2..].copy_from_slice(&[0xff, 0xd9]);
    jpeg
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}
