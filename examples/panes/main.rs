//! Panes from the Rust SDK: what opens each one, its inputs (some refused), context and close, and
//! a periodic task that keeps every open clock current. The twin of core's Python test extension
//! in tests/test_extension_panes.py.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::bail;
use buffa_types::google::protobuf::ListValue;
use pointman_extension::protocol::PaneOpenResponse;
use pointman_extension::v1::{
    block::Family, chart, input, layout, list, Block, Chart, Input, Layout, List, PaneContext, PaneInput, Text, Tone,
};
use pointman_extension::{Extension, Handle, Pane, Refused};
use serde_json::json;

fn text(name: &str, markdown: &str) -> Block {
    let family = Family::Text(Box::new(Text { markdown: markdown.into(), ..Default::default() }));
    Block { name: Some(name.into()), family: Some(family), ..Default::default() }
}

fn stat(name: &str, label: &str, value: i64, tone: Tone) -> Block {
    let data = chart::Data {
        columns: vec!["value".into()],
        rows: vec![ListValue::from_values([value as f64])],
        ..Default::default()
    };
    let chart = Chart {
        r#type: chart::Type::TYPE_STAT.into(),
        title: label.into(),
        data: data.into(),
        tone: tone.into(),
        ..Default::default()
    };
    Block { name: Some(name.into()), family: Some(Family::Chart(Box::new(chart))), ..Default::default() }
}

fn stack(name: &str, children: Vec<Block>) -> Block {
    let stack = layout::Stack { children, ..Default::default() };
    let layout = Layout { kind: Some(layout::Kind::Stack(Box::new(stack))), ..Default::default() };
    Block { name: Some(name.into()), family: Some(Family::Layout(Box::new(layout))), ..Default::default() }
}

fn plus(name: &str) -> Block {
    let button = input::actions::Button { id: "plus".into(), title: "+".into(), ..Default::default() };
    let actions = input::Actions { buttons: vec![button], ..Default::default() };
    let input = Input { kind: Some(input::Kind::Actions(Box::new(actions))), ..Default::default() };
    Block { name: Some(name.into()), family: Some(Family::Input(Box::new(input))), ..Default::default() }
}

fn log(name: &str, ticks: u64) -> Block {
    let items =
        (1..=ticks).map(|i| list::Item { id: format!("t{i}"), title: format!("Tick {i}"), ..Default::default() });
    let list = List { items: items.collect(), ..Default::default() };
    Block { name: Some(name.into()), family: Some(Family::List(Box::new(list))), ..Default::default() }
}

fn tone(n: i64) -> Tone {
    if n > 2 {
        Tone::TONE_GOOD
    } else {
        Tone::TONE_UNSPECIFIED
    }
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;

    ext.pane("counter", |pane: &Pane| {
        if pane.target()["fail"] == true {
            bail!("no such counter");
        }
        if pane.target()["bad"] == true {
            return Ok(PaneOpenResponse::default().titled("Counter"));
        }
        let start = pane.target()["start"].as_i64().unwrap_or(0);
        let n = Arc::new(AtomicI64::new(start));
        let seen = Arc::new(Mutex::new("Looking at nothing".to_string()));
        let view = {
            let (n, seen) = (n.clone(), seen.clone());
            move || {
                let n = n.load(Ordering::SeqCst);
                let seen = seen.lock().unwrap().clone();
                vec![stack("top", vec![stat("count", "Taps", n, tone(n)), text("seen", &seen)]), plus("plus")]
            }
        };

        let (taps, shows) = (n.clone(), view.clone());
        pane.on_input(move |pane: &Pane, event: &PaneInput| -> anyhow::Result<()> {
            match event.str("block").unwrap_or_default() {
                "plus" => {
                    taps.fetch_add(1, Ordering::SeqCst);
                    pane.show(shows());
                }
                "stop" => pane.close("stopped"),
                // an edit of a block that isn't there: the node would drop it
                "broken" => pane.edit(vec![pointman_extension::edits::remove("missing")]),
                _ => return Err(Refused::new("only + works here").into()),
            }
            Ok(())
        });
        let shows = view.clone();
        pane.on_context(move |pane: &Pane, context: &PaneContext| {
            let thread = context.thread.as_option().map_or("nothing", |t| t.id.as_str());
            *seen.lock().unwrap() = format!("Looking at {thread}");
            pane.show(shows());
        });
        let closing = n.clone();
        pane.on_close(move |pane: &Pane, reason: &str| {
            let n = closing.load(Ordering::SeqCst);
            pane.handle().event("panes.closed", json!({"pane": pane.id(), "reason": reason, "n": n}));
        });

        pane.set_title(&format!("Counter from {start}")); // sent while opening: it follows the answer
        Ok(PaneOpenResponse::blocks(view()).titled("Counter"))
    });

    ext.pane("board", |pane: &Pane| {
        let board = Arc::new(Mutex::new(json!({
            "project": {"key": "ALA", "title": "Spark", "leading_tool": "linear"},
            "states": [{"name": "In Progress", "category": "active"}, {"name": "Done", "category": "done"}],
            "items": [
                {"id": "lin_1", "ref": "ALA-1", "title": "One", "state": "In Progress", "category": "active"},
                {"id": "lin_parent", "ref": "ALA-2", "title": "Parent", "state": "In Progress", "category": "active"}]})));
        let now = board.clone();
        pane.on_input(move |pane: &Pane, event: &PaneInput| -> anyhow::Result<()> {
            let fields = event.json();
            let item = event.str("item").unwrap_or_default();
            match event.kind.as_str() {
                "move" => {
                    let to = event.str("to").unwrap_or_default();
                    if item == "lin_parent" && to == "Done" {
                        bail!(Refused::new("Linear decides when parents close"));
                    }
                    let category = if to == "Done" { "done" } else { "active" };
                    let mut board = now.lock().unwrap();
                    if let Some(it) = board["items"].as_array_mut().unwrap().iter_mut().find(|i| i["id"] == item) {
                        it["state"] = json!(to);
                        it["category"] = json!(category);
                    }
                    pane.show_board(&board);
                }
                // an edit changes the tool's copy; show_board sends only what changed
                "edit" => {
                    let mut board = now.lock().unwrap();
                    if let Some(it) = board["items"].as_array_mut().unwrap().iter_mut().find(|i| i["id"] == item) {
                        if let Some(fields) = fields["fields"].as_object() {
                            it.as_object_mut().unwrap().extend(fields.clone());
                        }
                    }
                    pane.show_board(&board);
                }
                kind => bail!(Refused::new(format!("{kind} isn't something this board does"))),
            }
            Ok(())
        });
        let first = board.lock().unwrap().clone();
        Ok(PaneOpenResponse::board(&first).titled("Spark"))
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
                pane.show(vec![stat("ticks", "Ticks", *n as i64, Tone::TONE_UNSPECIFIED), log("log", *n)]);
            }
        },
        "clock",
    );
    ext.pane("clock", |_pane: &Pane| {
        Ok(PaneOpenResponse::blocks(vec![stat("ticks", "Ticks", 0, Tone::TONE_UNSPECIFIED), log("log", 0)]))
    });

    ext.pane("page", |_pane: &Pane| Ok(PaneOpenResponse::page("pages/index.html").titled("A page")));

    ext.run()
}
