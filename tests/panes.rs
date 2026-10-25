//! The panes example against the stand-in node (Pointman's docs/dev/panes.md): an extension answers
//! pane.open with its first content and keeps it current with set and edit, takes inputs in order
//! and refuses some, and hears context and close. The twin of core's tests/test_extension_panes.py.

mod common;

use std::time::Duration;

use common::{example, source};
use pointman_extension::testing::{PaneOptions, PaneView, StandIn};
use pointman_extension::v1::{PaneContext, PaneFormat, ThreadId};
use pointman_extension::Extension;
use serde_json::{json, Value};

const WAIT: Duration = Duration::from_secs(10);

fn panes() -> StandIn {
    StandIn::new(source("panes")).binary(example("panes")).start().unwrap()
}

fn tap(block: &str) -> Value {
    json!({"kind": "tap", "block": block})
}

fn json_of(block: Option<pointman_extension::v1::Block>) -> Value {
    serde_json::to_value(block.unwrap()).unwrap()
}

/// A stat's number, from its first cell.
fn count(block: Option<pointman_extension::v1::Block>) -> Value {
    json_of(block)["chart"]["data"]["rows"][0][0].clone()
}

#[test]
fn a_pane_opens_with_its_manifest_format_and_inputs_edit_it() {
    let node = panes();
    let thread = ThreadId { value: "thr_1".into(), ..Default::default() };
    let options =
        PaneOptions { context: PaneContext { thread: thread.into(), ..Default::default() }, ..Default::default() };
    let view = node.open_pane_with("counter", json!({"start": 1}), options).unwrap();
    assert_eq!(view.format(), Some(PaneFormat::PANE_FORMAT_BLOCKS));
    let names: Vec<String> = view.blocks().iter().map(|b| b.name.clone().unwrap()).collect();
    assert_eq!(names, ["top", "plus"]);
    assert_eq!(count(view.block("count")), 1.0); // inside a stack: found by name
                                                 // the title it set while opening came after the answer, not before
    view.wait(WAIT, |v| v.title().as_deref() == Some("Counter from 1")).unwrap();
    assert_eq!(
        view.frames()[0],
        ("pane.title".to_string(), json!({"pane": {"value": view.id()}, "title": "Counter from 1"}))
    );

    for _ in 0..3 {
        view.input_json(tap("plus")).unwrap(); // its edits are applied by the answer
    }
    assert_eq!(count(view.block("count")), 4.0);
    assert_eq!(json_of(view.block("count"))["chart"]["tone"], "TONE_GOOD");
    // show sent the stat alone, by its name: not the stack it is in, nor the button
    let last = view.frames().pop().unwrap();
    assert_eq!(last.0, "pane.edit");
    let edits = last.1["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0]["replace"]["block"]["name"], "count");
    assert!(view.problems().is_empty(), "{:?}", view.problems());
}

#[test]
fn a_refused_input_says_why_and_changes_nothing() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    let before = view.content();
    assert_eq!(view.input_json(tap("minus")).unwrap_err().message, "only + works here");
    assert_eq!(view.content(), before);

    let board = node.open_pane("board", Value::Null).unwrap();
    board.input_json(json!({"kind": "move", "item": "lin_1", "to": "Done"})).unwrap();
    assert_eq!(board.item("lin_1").unwrap()["state"], "Done");
    assert_eq!(board.item("lin_1").unwrap()["category"], "done");
    let refused = board.input_json(json!({"kind": "move", "item": "lin_parent", "to": "Done"})).unwrap_err();
    assert_eq!(refused.message, "Linear decides when parents close");
    assert_eq!(board.item("lin_parent").unwrap()["state"], "In Progress");
    let refused = board.input_json(json!({"kind": "rank", "item": "lin_1", "before": "lin_parent"})).unwrap_err();
    assert_eq!(refused.message, "rank isn't something this board does");

    // a pane that never said what it takes refuses everything
    let clock = node.open_pane("clock", Value::Null).unwrap();
    assert_eq!(clock.input_json(tap("ticks")).unwrap_err().message, "this pane takes no input");
    assert!(view.problems().is_empty() && board.problems().is_empty());
}

#[test]
fn context_and_close_reach_the_pane() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    let thread = ThreadId { value: "thr_9".into(), ..Default::default() };
    view.look_at(PaneContext { thread: thread.into(), ..Default::default() }).unwrap();
    view.wait(WAIT, |v| v.block("seen").is_some_and(|b| json_of(Some(b))["text"]["markdown"] == "Looking at thr_9"))
        .unwrap();
    view.input_json(tap("plus")).unwrap();
    view.close("idle").unwrap();
    let closed = node.wait_event("panes.closed", WAIT).unwrap();
    assert_eq!(closed, json!({"pane": view.id(), "reason": "idle", "n": 1}));
    let refused = view.input_json(tap("plus")).unwrap_err();
    assert_eq!(refused.message, format!("no pane {} is open", view.id()));
}

#[test]
fn the_extension_closes_a_pane_itself() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    view.input_json(tap("stop")).unwrap();
    view.wait(WAIT, |v| v.closed().as_deref() == Some("stopped")).unwrap();
    let closed = ("pane.closed".to_string(), json!({"pane": {"value": view.id()}, "reason": "stopped"}));
    assert!(view.frames().contains(&closed), "{:?}", view.frames());
    assert_eq!(node.wait_event("panes.closed", WAIT).unwrap()["reason"], "stopped");
    assert!(view.input_json(tap("plus")).unwrap_err().message.starts_with("no pane"));
}

#[test]
fn a_task_keeps_every_open_pane_current() {
    let node = panes();
    let (a, b) = (node.open_pane("clock", Value::Null).unwrap(), node.open_pane("clock", Value::Null).unwrap());
    let ticks = |v: &PaneView| count(v.block("ticks")).as_f64().map(|n| n as u64);
    a.wait(WAIT, |v| ticks(v) >= Some(3)).unwrap();
    b.wait(WAIT, |v| ticks(v) >= Some(3)).unwrap();
    a.close("closed").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let stopped_at = a.frames().len();
    b.wait(WAIT, |v| ticks(v) >= Some(8)).unwrap();
    assert_eq!(a.frames().len(), stopped_at); // a closed pane hears nothing more
    assert!(a.title().is_none() && a.problems().is_empty() && b.problems().is_empty());
    // show sent only what changed: each tick the count, and the log's new row
    let frames = b.frames();
    assert!(frames.iter().all(|(method, _)| method == "pane.edit"), "{frames:?}");
    let edits = frames[1].1["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 2, "{edits:?}");
    assert_eq!(edits[0]["replace"]["block"]["name"], "ticks");
    assert_eq!(edits[1]["putItems"]["block"], "log");
    assert_eq!(edits[1]["putItems"]["items"], json!([{"list": {"id": "t2", "title": "Tick 2"}}]));
    let shown = ticks(&b).unwrap() as usize;
    let log = json_of(b.block("log"));
    assert!(log["list"]["items"].as_array().unwrap().len() >= shown.min(8));
}

#[test]
fn show_board_sends_only_the_items_that_changed() {
    let node = panes();
    let board = node.open_pane("board", Value::Null).unwrap();
    board.input_json(json!({"kind": "move", "item": "lin_1", "to": "Done"})).unwrap();
    board.input_json(json!({"kind": "edit", "item": "lin_parent", "fields": {"title": "Parent, renamed"}})).unwrap();
    let frames = board.frames();
    assert_eq!(frames.len(), 2);
    // the move sent its item; the edit's show sends the renamed one alone
    assert_eq!(frames[0].0, "pane.edit");
    let item = &frames[1].1["edits"][0]["putItems"]["items"];
    assert_eq!(frames[1].1["edits"].as_array().unwrap().len(), 1);
    assert_eq!(item.as_array().unwrap().len(), 1);
    assert_eq!(item[0]["board"]["id"], "lin_parent");
    assert_eq!(item[0]["board"]["title"], "Parent, renamed");
    assert!(frames[1].1["edits"][0]["putItems"].get("block").is_none(), "a board's items name no block");
    assert_eq!(board.item("lin_parent").unwrap()["title"], "Parent, renamed");
    assert_eq!(board.item("lin_1").unwrap()["state"], "Done");
    assert!(board.problems().is_empty(), "{:?}", board.problems());
}

#[test]
fn panes_that_dont_open() {
    let node = panes();
    assert_eq!(node.open_pane("counter", json!({"fail": true})).err().unwrap().message, "no such counter");
    let bad = node.open_pane("counter", json!({"bad": true})).err().unwrap();
    assert_eq!(bad.message, "a blocks pane opens with its content, as blocks");
    assert_eq!(node.open_pane("nope", Value::Null).err().unwrap().message, "nope isn't a pane of panes");
    let page = node.open_pane("page", Value::Null).unwrap();
    assert_eq!(
        (page.format(), page.path(), page.title()),
        (Some(PaneFormat::PANE_FORMAT_PAGE), Some("pages/index.html".into()), Some("A page".into()))
    );
    // the extension carries on
    assert_eq!(count(node.open_pane("counter", Value::Null).unwrap().block("count")), 0.0);
}

#[test]
fn the_stand_in_says_what_the_node_would_refuse() {
    let node = panes();
    let view = node.open_pane("counter", Value::Null).unwrap();
    view.input_json(tap("broken")).unwrap();
    assert_eq!(view.problems(), ["edits: edit 1: no block \"missing\""]);
    assert_eq!(count(view.block("count")), 0.0); // all or none: nothing applied
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
        ext.pane("other", |_pane: &pointman_extension::Pane| Ok(Default::default()));
    }));
    assert!(panicked.is_err());
}
