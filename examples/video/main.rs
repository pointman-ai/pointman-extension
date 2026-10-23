//! A video pane from the Rust SDK: it opens with its size and rate (and its sound's, with
//! `{"audio": true}`), sends frames and sound only while the node says a path is attached, and follows
//! the app's hints (`max_fps: 0` pauses the pictures, `audio: false` the sound), and takes pointer and
//! key inputs only while the node says a person has control. Its frames stand in
//! for a camera's JPEGs: a JPEG's first and last markers around a count; its sound is ADTS frames of
//! silence's length, headers and zeros.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pointman_extension::protocol::pane_open_response::{video, Video};
use pointman_extension::protocol::{PaneControlRequest, PaneOpenResponse};
use pointman_extension::v1::{member_ref, PaneInput};
use pointman_extension::{Extension, Pane, Refused, VIDEO_FRAME_MAX};
use serde_json::{json, Value};

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;

    ext.pane("live", |pane: &Pane| {
        if pane.target()["bad"] == true {
            return Ok(PaneOpenResponse::default().titled("Test card")); // no video: the open fails
        }
        let size = if pane.target()["big"] == true { VIDEO_FRAME_MAX } else { 16 };
        // before the open is answered there's nobody to send it to: dropped
        let early = pane.frame(&frame(0, 16), 64, 48, now_ms());
        pane.handle().event("video.early", json!({"sent": early}));

        let fps = Arc::new(AtomicU64::new(30));
        let speaker = Arc::new(AtomicBool::new(true));
        let running: Arc<Mutex<Option<Arc<AtomicBool>>>> = Arc::default();
        // with audio, it says so; undeclared, it sends sound anyway, which the node would refuse
        let with_audio = pane.target()["audio"] == true;
        let sounds = with_audio || pane.target()["undeclared"] == true;

        let (rate, feed, on) = (fps.clone(), running.clone(), speaker.clone());
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
                if sounds {
                    let (pane, on, stop) = (pane.clone(), on.clone(), stop.clone());
                    thread::spawn(move || speak(&pane, &on, &stop));
                }
                thread::spawn(move || send(&pane, &rate, &stop, size));
            }
            pane.handle().event("video.watching", json!({"attached": attached, "viewers": pane.viewers()}));
        });
        // control: a grant from `by` until a time, which a target of {"locked": true} turns down (as a
        // machine without the permission would); while it holds, pointer and key inputs are taken
        let controlled: Arc<Mutex<Option<Value>>> = Arc::default();
        let (held, locked) = (controlled.clone(), pane.target()["locked"] == true);
        pane.on_control(move |pane: &Pane, grant: Option<&PaneControlRequest>| -> anyhow::Result<()> {
            if grant.is_some() && locked {
                return Err(Refused::new("this test card can't be controlled").into());
            }
            let grant = grant.map(|g| {
                let by = match g.by.as_option().and_then(|m| m.kind.as_ref()) {
                    Some(member_ref::Kind::Handle(handle)) => handle.clone(),
                    _ => String::new(),
                };
                json!({"until": g.until.seconds, "by": by, "device": g.device})
            });
            *held.lock().unwrap() = grant.clone();
            pane.handle().event("video.control", json!({"grant": grant}));
            Ok(())
        });
        pane.on_input(move |pane: &Pane, event: &PaneInput| -> anyhow::Result<()> {
            if matches!(event.kind.as_str(), "pointer" | "scroll" | "key" | "text") {
                if controlled.lock().unwrap().is_none() {
                    return Err(Refused::new("control is off").into());
                }
                let mut seen = event.json();
                seen["kind"] = json!(event.kind);
                pane.handle().event("video.controlled", seen);
                return Ok(());
            }
            if event.kind != "hint" {
                return Err(Refused::new("a video pane takes hints only").into());
            }
            // a hint says only what changed: the speaker's alone has no rate
            if let Some(max) = event.json()["max_fps"].as_f64() {
                fps.store((max.ceil() as u64).min(30), Ordering::SeqCst);
            }
            if let Some(on) = event.json()["audio"].as_bool() {
                speaker.store(on, Ordering::SeqCst);
            }
            Ok(())
        });
        pane.on_close(move |pane: &Pane, reason: &str| {
            if let Some(stop) = running.lock().unwrap().take() {
                stop.store(true, Ordering::SeqCst);
            }
            pane.handle().event("video.closed", json!({"pane": pane.id(), "reason": reason}));
        });
        let audio = with_audio.then(|| video::Audio {
            codec: video::audio::Codec::CODEC_AAC.into(),
            rate: 48000,
            channels: 1,
            ..Default::default()
        });
        let video = Video {
            codec: video::Codec::CODEC_MJPEG.into(),
            width: 64,
            height: 48,
            fps: 30.0,
            audio: audio.into(),
            ..Default::default()
        };
        Ok(PaneOpenResponse::video(video).titled("Test card"))
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

/// Sound while the speaker's on, two ADTS frames (2048 samples at 48 kHz, 43 ms) at a time, timed by its
/// samples from when it started, until stopped.
fn speak(pane: &Pane, on: &AtomicBool, stop: &AtomicBool) {
    let (start, mut samples) = (now_ms(), 0u64);
    while !stop.load(Ordering::SeqCst) {
        if on.load(Ordering::SeqCst) {
            pane.audio(&[adts(40), adts(56)].concat(), start + samples * 1000 / 48000);
            samples += 2048;
        }
        thread::sleep(Duration::from_millis(43));
    }
}

/// An ADTS frame of `len` bytes: its header (AAC-LC, 48 kHz, mono, no CRC), then zeros.
fn adts(len: usize) -> Vec<u8> {
    let mut f = vec![0xff, 0xf1, 0x4c, 0x40 | (len >> 11) as u8, (len >> 3) as u8, ((len & 7) << 5) as u8 | 0x1f, 0xfc];
    f.resize(len, 0);
    f
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
