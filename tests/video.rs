//! The video example against the stand-in node (Pointman's docs/dev/video.md): a video pane opens with
//! its size and rate, sends frames only while a path is attached, follows the app's hints, and stops
//! when it closes.

mod common;

use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{example, source};
use pointman_extension::testing::{PaneView, StandIn};
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);

fn video() -> StandIn {
    StandIn::new(source("video")).binary(example("video")).start().unwrap()
}

/// The count of frames over a while: the same at both ends means none came.
fn still_for(view: &PaneView, quiet: Duration) -> bool {
    let before = view.frame_count();
    sleep(quiet);
    view.frame_count() == before
}

#[test]
fn a_video_pane_opens_with_its_size_and_sends_frames_only_while_watched() {
    let node = video();
    let view = node.open_pane("live", Value::Null).unwrap();
    assert_eq!(view.format().as_deref(), Some("video"));
    assert_eq!(view.title().as_deref(), Some("Test card"));
    assert_eq!(view.video(), json!({"codec": "mjpeg", "width": 64, "height": 48, "fps": 30}));
    // the frame it sent while opening went nowhere, and nothing comes before a path attaches
    assert_eq!(node.wait_event("video.early", WAIT).unwrap(), json!({"sent": false}));
    assert!(still_for(&view, Duration::from_millis(300)));

    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 5).unwrap();
    assert_eq!(node.wait_event("video.watching", WAIT).unwrap(), json!({"attached": 1, "viewers": 1}));
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let frames = view.video_frames();
    for (i, frame) in frames.iter().enumerate() {
        assert_eq!((frame.width, frame.height, frame.jpeg.len()), (64, 48, 16));
        assert!(frame.jpeg.starts_with(&[0xff, 0xd8]) && frame.jpeg.ends_with(&[0xff, 0xd9]));
        assert!(frame.captured_ms <= now && frame.captured_ms > now - 10_000, "{}", frame.captured_ms);
        if i > 0 {
            assert!(frame.jpeg[2..10] > frames[i - 1].jpeg[2..10], "frames arrive in order");
        }
    }
    // a second path and then one fewer: it keeps sending; none left: it stops
    view.viewers(2).unwrap();
    view.viewers(1).unwrap();
    node.wait_event_where("video.watching", WAIT, |w| w["attached"] == 1 && w["viewers"] == 1).unwrap();
    assert_eq!(node.events()["video.capturing"], json!({"on": true}));
    view.viewers(0).unwrap();
    node.wait_event_where("video.capturing", WAIT, |c| c["on"] == false).unwrap();
    assert!(still_for(&view, Duration::from_millis(300)));
    assert!(view.problems().is_empty(), "{:?}", view.problems());
    // nothing else went to the app: no sets or patches
    assert!(view.frames().is_empty(), "{:?}", view.frames());
}

#[test]
fn a_hint_of_zero_pauses_it_and_another_starts_it_again() {
    let node = video();
    let view = node.open_pane("live", Value::Null).unwrap();
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 2).unwrap();
    view.hint(0, 1280).unwrap();
    sleep(Duration::from_millis(100)); // one already on its way
    assert!(still_for(&view, Duration::from_millis(300)));
    let paused = view.frame_count();
    view.hint(10, 640).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= paused + 2).unwrap();
    // ten a second is about what it sends now
    let (start, from) = (Instant::now(), view.frame_count());
    sleep(Duration::from_millis(1000));
    let rate = (view.frame_count() - from) as f64 / start.elapsed().as_secs_f64();
    assert!((5.0..=12.0).contains(&rate), "{rate} frames a second");
    let refused = view.input(json!({"kind": "tap", "block": "x"})).unwrap_err();
    assert_eq!(refused.message, "a video pane takes hints only");
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn closing_it_stops_its_frames() {
    let node = video();
    let view = node.open_pane("live", Value::Null).unwrap();
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.frame_count() >= 2).unwrap();
    view.close("no_viewer").unwrap();
    assert_eq!(node.wait_event("video.closed", WAIT).unwrap(), json!({"pane": view.id(), "reason": "no_viewer"}));
    node.wait_event_where("video.capturing", WAIT, |c| c["on"] == false).unwrap();
    let after = view.frame_count();
    sleep(Duration::from_millis(200));
    assert_eq!(view.frame_count(), after);
    assert!(view.viewers(1).is_ok() && view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn a_frame_too_big_once_sealed_is_dropped_and_logged_once() {
    let node = video();
    let view = node.open_pane("live", json!({"big": true})).unwrap();
    view.viewers(1).unwrap();
    let deadline = Instant::now() + WAIT;
    while !node.log().contains("over the 4 MiB a frame") && Instant::now() < deadline {
        sleep(Duration::from_millis(20));
    }
    sleep(Duration::from_millis(300));
    assert_eq!(node.log().matches("over the 4 MiB a frame").count(), 1, "{}", node.log());
    assert_eq!(view.frame_count(), 0);
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn a_video_pane_must_open_with_its_video() {
    let node = video();
    let e = node.open_pane("live", json!({"bad": true})).err().unwrap();
    assert!(e.message.contains("a video pane opens with its video"), "{}", e.message);
}

#[test]
fn sound_comes_while_watched_with_the_speaker_on() {
    let node = video();
    let view = node.open_pane("live", json!({"audio": true})).unwrap();
    assert_eq!(view.video()["audio"], json!({"codec": "aac", "rate": 48000, "channels": 1}));
    sleep(Duration::from_millis(200));
    assert_eq!(view.sound_count(), 0);
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| v.sound_count() >= 5 && v.frame_count() >= 5).unwrap();
    let sounds = view.sounds();
    assert!(sounds.iter().all(|s| s.frames == 2 && s.aac.len() == 96), "{sounds:?}");
    // in order, 2048 samples (42 ms) apart
    for pair in sounds.windows(2) {
        assert!((42..=43).contains(&(pair[1].captured_ms - pair[0].captured_ms)), "{pair:?}");
    }
    // the speaker off: sound stops, pictures don't, and the rate stays as it was
    view.audio_hint(false).unwrap();
    sleep(Duration::from_millis(100));
    assert!(still_for_sound(&view, Duration::from_millis(300)));
    assert!(!still_for(&view, Duration::from_millis(300)));
    view.audio_hint(true).unwrap();
    let back = view.sound_count();
    view.wait(WAIT, |v| v.sound_count() > back + 2).unwrap();
    view.viewers(0).unwrap();
    sleep(Duration::from_millis(100));
    assert!(still_for_sound(&view, Duration::from_millis(300)));
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn sound_from_a_pane_that_opened_without_it_is_refused() {
    let node = video();
    let view = node.open_pane("live", json!({"undeclared": true})).unwrap();
    view.viewers(1).unwrap();
    view.wait(WAIT, |v| !v.problems().is_empty()).unwrap();
    assert_eq!(view.problems()[0], "pane.audio on a video pane that opened without audio");
    assert_eq!(view.sound_count(), 0);
}

/// The count of sound over a while: the same at both ends means none came.
fn still_for_sound(view: &PaneView, quiet: Duration) -> bool {
    let before = view.sound_count();
    sleep(quiet);
    view.sound_count() == before
}
