//! The panes example against the stand-in node (Pointman's docs/dev/panes.md): an extension answers
//! pane.open with its first content and keeps it current with set and patch, takes inputs in order
//! and refuses some, and hears context and close. The twin of core's tests/test_extension_panes.py.

mod common;

use std::time::Duration;

use common::{example, source};
use pointman_extension::testing::{PaneOptions, StandIn};
use pointman_extension::Extension;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);

fn panes() -> StandIn {
    StandIn::new(source("panes")).binary(example("panes")).start().unwrap()
}

fn tap(block: &str) -> Value {
    json!({"kind": "tap", "block": block, "value": null})
}

#[test]
fn a_pane_opens_with_its_manifest_format_and_inputs_patch_it() {
    let node = panes();
    let options = PaneOptions { context: json!({"thread": "thr_1"}), ..Default::default() };
    let view = node.open_pane_with("counter", json!({"start": 1}), options).unwrap();
    assert_eq!(view.format().as_deref(), Some("blocks"));
    let ids: Vec<Value> = view.content().as_array().unwrap().iter().map(|b| b["id"].clone()).collect();
    assert_eq!(ids, [json!("top"), json!("plus")]);
    assert_eq!(view.block("count").unwrap()["value"], 1); // inside a stack: found by id
                                                          // the title it set while opening came after the answer, not before
    view.wait(WAIT, |v| v.title().as_deref() == Some("Counter from 1")).unwrap();
    assert_eq!(view.frames()[0], ("pane.title".to_string(), json!({"pane": view.id(), "title": "Counter from 1"})));

    for _ in 0..3 {
        view.input(tap("plus")).unwrap(); // its patch is applied by the answer
    }
    assert_eq!(
        view.block("count").unwrap(),
        json!({"type": "stat", "id": "count", "label": "Taps", "value": 4, "tone": "good"})
    );
    let last = view.frames().pop().unwrap();
    assert_eq!(
        last,
        (
            "pane.patch".to_string(),
            json!({"pane": view.id(), "patch": [
            {"op": "add", "path": "/b/count/tone", "value": "good"},
            {"op": "add", "path": "/b/count/value", "value": 4}]})
        )
    );
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn a_refused_input_says_why_and_changes_nothing() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    let before = view.content();
    assert_eq!(view.input(tap("minus")).unwrap_err().message, "only + works here");
    assert_eq!(view.content(), before);

    let board = node.open_pane("board", Value::Null).unwrap();
    board.input(json!({"kind": "move", "item": "lin_1", "to": "Done"})).unwrap();
    assert_eq!(board.item("lin_1").unwrap()["state"], "Done");
    assert_eq!(board.item("lin_1").unwrap()["category"], "done");
    let refused = board.input(json!({"kind": "move", "item": "lin_parent", "to": "Done"})).unwrap_err();
    assert_eq!(refused.message, "Linear decides when parents close");
    assert_eq!(board.item("lin_parent").unwrap()["state"], "In Progress");
    let refused = board.input(json!({"kind": "rank", "item": "lin_1", "before": "lin_parent"})).unwrap_err();
    assert_eq!(refused.message, "rank isn't something this board does");

    // a pane that never said what it takes refuses everything
    let clock = node.open_pane("clock", Value::Null).unwrap();
    assert_eq!(clock.input(tap("ticks")).unwrap_err().message, "this pane takes no input");
    assert!(view.problems().is_empty() && board.problems().is_empty());
}

#[test]
fn context_and_close_reach_the_pane() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    view.look_at(json!({"thread": "thr_9"})).unwrap();
    view.wait(WAIT, |v| v.block("seen").is_some_and(|b| b["markdown"] == "Looking at thr_9")).unwrap();
    view.input(tap("plus")).unwrap();
    view.close("idle").unwrap();
    let closed = node.wait_event("panes.closed", WAIT).unwrap();
    assert_eq!(closed, json!({"pane": view.id(), "reason": "idle", "n": 1}));
    let refused = view.input(tap("plus")).unwrap_err();
    assert_eq!(refused.message, format!("no pane {} is open", view.id()));
}

#[test]
fn the_extension_closes_a_pane_itself() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    view.input(tap("stop")).unwrap();
    view.wait(WAIT, |v| v.closed().as_deref() == Some("stopped")).unwrap();
    assert!(view.frames().contains(&("pane.closed".to_string(), json!({"pane": view.id(), "reason": "stopped"}))));
    assert_eq!(node.wait_event("panes.closed", WAIT).unwrap()["reason"], "stopped");
    assert!(view.input(tap("plus")).unwrap_err().message.starts_with("no pane"));
}

#[test]
fn a_task_keeps_every_open_pane_current() {
    let node = panes();
    let (a, b) = (node.open_pane("clock", Value::Null).unwrap(), node.open_pane("clock", Value::Null).unwrap());
    let ticks = |v: &pointman_extension::testing::PaneView| v.block("ticks").and_then(|b| b["value"].as_u64());
    a.wait(WAIT, |v| ticks(v) >= Some(3)).unwrap();
    b.wait(WAIT, |v| ticks(v) >= Some(3)).unwrap();
    a.close("closed").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let stopped_at = a.frames().len();
    b.wait(WAIT, |v| ticks(v) >= Some(8)).unwrap();
    assert_eq!(a.frames().len(), stopped_at); // a closed pane hears nothing more
    assert!(a.title().is_none() && a.problems().is_empty() && b.problems().is_empty());
}

#[test]
fn panes_that_dont_open() {
    let node = panes();
    assert_eq!(node.open_pane("counter", json!({"fail": true})).err().unwrap().message, "no such counter");
    let bad = node.open_pane("counter", json!({"bad": true})).err().unwrap();
    assert_eq!(bad.message, "a blocks pane opens with its blocks, an array");
    assert_eq!(node.open_pane("nope", Value::Null).err().unwrap().message, "nope isn't a pane of panes");
    let page = node.open_pane("page", Value::Null).unwrap();
    assert_eq!(
        (page.format(), page.path(), page.title()),
        (Some("page".into()), Some("pages/index.html".into()), Some("A page".into()))
    );
    // the extension carries on
    assert_eq!(node.open_pane("counter", Value::Null).unwrap().block("count").unwrap()["value"], 0);
}

#[test]
fn the_stand_in_says_what_the_node_would_refuse() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    view.input(tap("broken")).unwrap();
    assert_eq!(view.problems(), ["patch: '/b/missing/value': no block 'missing'"]);
    assert_eq!(view.block("count").unwrap()["value"], 0);
}

#[test]
fn a_pane_must_be_in_the_manifest() {
    let ext = Extension::from_manifest(source("panes")).unwrap();
    let formats = ext.pane_formats();
    assert_eq!(formats.len(), 4);
    assert_eq!(formats["board"].as_deref(), Some("board"));
    assert_eq!(formats["page"].as_deref(), Some("page"));
    let mut ext = ext;
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ext.pane("other", |_pane: &pointman_extension::Pane| Ok(json!({})));
    }));
    assert!(panicked.is_err());
}
