//! Panes from the Rust SDK: what opens each one, its inputs (some refused), context and close, and
//! a periodic task that keeps every open clock current. The twin of core's Python test extension
//! in tests/test_extension_panes.py.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use pointman_extension::{Extension, Handle, Pane, Refused};
use serde_json::{json, Value};

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;

    ext.pane("counter", |pane: &Pane| {
        if pane.target()["fail"] == true {
            bail!("no such counter");
        }
        if pane.target()["bad"] == true {
            return Ok(json!({"title": "Counter"}));
        }
        let start = pane.target()["start"].as_i64().unwrap_or(0);
        let n = Arc::new(AtomicI64::new(start));

        let taps = n.clone();
        pane.on_input(move |pane: &Pane, event: &Value| -> anyhow::Result<()> {
            match event["block"].as_str().unwrap_or_default() {
                "plus" => {
                    let n = taps.fetch_add(1, Ordering::SeqCst) + 1;
                    pane.update("count", json!({"value": n, "tone": if n > 2 { json!("good") } else { Value::Null }}));
                }
                "stop" => pane.close("stopped"),
                "broken" => pane.patch(json!([{"op": "replace", "path": "/b/missing/value", "value": 1}])),
                _ => return Err(Refused::new("only + works here").into()),
            }
            Ok(())
        });
        pane.on_context(|pane: &Pane, context: &Value| {
            let thread = context["thread"].as_str().unwrap_or("nothing");
            pane.update("seen", json!({"markdown": format!("Looking at {thread}")}));
        });
        let closing = n.clone();
        pane.on_close(move |pane: &Pane, reason: &str| {
            let n = closing.load(Ordering::SeqCst);
            pane.handle().event("panes.closed", json!({"pane": pane.id(), "reason": reason, "n": n}));
        });

        pane.set_title(&format!("Counter from {start}")); // sent while opening: it follows the answer
        Ok(json!({"title": "Counter", "blocks": [
            {"type": "stack", "id": "top", "children": [
                {"type": "stat", "id": "count", "label": "Taps", "value": start},
                {"type": "text", "id": "seen", "markdown": "Looking at nothing"}]},
            {"type": "actions", "id": "plus", "buttons": [{"id": "plus", "title": "+"}]}]}))
    });

    ext.pane("board", |pane: &Pane| {
        let board = Arc::new(Mutex::new(json!({
            "project": {"key": "ALA", "title": "Spark", "leading_tool": "linear"},
            "states": [{"name": "In Progress", "category": "started"}, {"name": "Done", "category": "done"}],
            "items": [
                {"id": "lin_1", "ref": "ALA-1", "title": "One", "state": "In Progress", "category": "started"},
                {"id": "lin_parent", "ref": "ALA-2", "title": "Parent", "state": "In Progress", "category": "started"}]})));
        let now = board.clone();
        pane.on_input(move |pane: &Pane, event: &Value| -> anyhow::Result<()> {
            let kind = event["kind"].as_str().unwrap_or_default();
            let item = event["item"].as_str().unwrap_or_default();
            match kind {
                "move" => {
                    let to = event["to"].as_str().unwrap_or_default();
                    if item == "lin_parent" && to == "Done" {
                        bail!(Refused::new("Linear decides when parents close"));
                    }
                    let category = if to == "Done" { "done" } else { "started" };
                    pane.update_item(item, json!({"state": to, "category": category}));
                    let mut board = now.lock().unwrap();
                    if let Some(it) = board["items"].as_array_mut().unwrap().iter_mut().find(|i| i["id"] == item) {
                        it["state"] = json!(to);
                        it["category"] = json!(category);
                    }
                }
                // an edit changes the tool's copy; show_board sends only what changed
                "edit" => {
                    let mut board = now.lock().unwrap();
                    if let Some(it) = board["items"].as_array_mut().unwrap().iter_mut().find(|i| i["id"] == item) {
                        if let Some(fields) = event["fields"].as_object() {
                            it.as_object_mut().unwrap().extend(fields.clone());
                        }
                    }
                    pane.show_board(&*board);
                }
                _ => bail!(Refused::new(format!("{kind} isn't something this board does"))),
            }
            Ok(())
        });
        let first = board.lock().unwrap().clone();
        Ok(json!({"title": "Spark", "board": first}))
    });

    // each tick shows all of the clock; show sends only what changed: the count, and the log's new row
    let ticks: Mutex<HashMap<String, u64>> = Mutex::default();
    ext.every(
        Duration::from_millis(50),
        move |h: &Handle| {
            let mut ticks = ticks.lock().unwrap();
            for pane in h.panes("clock") {
                let n = ticks.entry(pane.id().to_string()).or_insert(0);
                *n += 1;
                let log: Vec<Value> =
                    (1..=*n).map(|i| json!({"id": format!("t{i}"), "title": format!("Tick {i}")})).collect();
                pane.show(json!([{"type": "stat", "id": "ticks", "label": "Ticks", "value": *n},
                                 {"type": "list", "id": "log", "items": log}]));
            }
        },
        "clock",
    );
    ext.pane("clock", |_pane: &Pane| {
        Ok(json!({"blocks": [{"type": "stat", "id": "ticks", "label": "Ticks", "value": 0},
                             {"type": "list", "id": "log", "items": []}]}))
    });

    ext.pane("page", |_pane: &Pane| Ok(json!({"title": "A page", "path": "pages/index.html"})));

    ext.run()
}
