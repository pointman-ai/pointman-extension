//! Blocks for a pane, built in a line each: the families extensions use most, as core's `typed/blocks.py` makes them
//! from today's JSON kinds (a stat, a meter and a ring are all a [`Chart`] drawn as a stat; a heading is a [`Text`]
//! with a level; a stack and tabs are a [`Layout`]).
//!
//! Every block in a pane has a `name`, unique there: edits and inputs say it, and [`Pane::show`](crate::Pane::show)
//! sends only what changed, by name.
//!
//! ```
//! use pointman_extension::blocks::{actions, button, heading, meter, stack};
//! use pointman_extension::v1::Tone;
//!
//! let account = stack("account:work", vec![
//!     heading("name:work", 3, "work", Some("runs start here")),
//!     meter("5h:work", "5-hour", 42.0, 100.0, Some("resets 14:00"), Tone::TONE_GOOD),
//! ]);
//! let swap = actions("actions", vec![button("swap", "Swap now")]);
//! assert_eq!(account.name.as_deref(), Some("account:work"));
//! assert!(swap.family.is_some());
//! ```

use buffa_types::google::protobuf::ListValue;

use crate::v1::block::Family;
use crate::v1::{chart, input, layout, list, Block, Chart, Code, Empty, Input, Layout, List, Text, Tone};

fn named(name: &str, family: Family) -> Block {
    Block { name: Some(name.to_string()), family: Some(family), ..Default::default() }
}

/// Markdown text.
pub fn text(name: &str, markdown: &str) -> Block {
    named(name, Family::Text(Box::new(Text { markdown: markdown.to_string(), ..Default::default() })))
}

/// A heading, level 1 to 3, with a quieter line under it.
pub fn heading(name: &str, level: i32, title: &str, subtitle: Option<&str>) -> Block {
    let heading = crate::v1::text::Heading { level, subtitle: subtitle.map(String::from), ..Default::default() };
    named(
        name,
        Family::Text(Box::new(Text {
            markdown: title.to_string(),
            heading: Some(heading).into(),
            ..Default::default()
        })),
    )
}

/// A number with a label, and what's beside it (a delta, beneath the number).
pub fn stat(name: &str, label: &str, value: f64, beside: Option<&str>, tone: Tone) -> Block {
    named(name, Family::Chart(Box::new(stat_chart(label, value, ("delta", beside), tone))))
}

/// A stat drawn as a bar against its `max`.
pub fn meter(name: &str, label: &str, value: f64, max: f64, note: Option<&str>, tone: Tone) -> Block {
    let mut chart = stat_chart(label, value, ("note", note), tone);
    chart.options = Some(chart::Options { max: Some(max), ..Default::default() }).into();
    named(name, Family::Chart(Box::new(chart)))
}

/// A stat drawn as a ring, filled `fill` (0 to 1) of the way, of `of`.
pub fn ring(name: &str, title: &str, value: f64, of: f64, fill: f64, detail: Option<&str>, tone: Tone) -> Block {
    let mut chart = stat_chart(title, value, ("detail", detail), tone);
    chart.options = Some(chart::Options { ring: true, max: Some(of), fill: Some(fill), ..Default::default() }).into();
    named(name, Family::Chart(Box::new(chart)))
}

/// A stat's chart: its value in the one row, and what's beside it (a stat's delta, a meter's note, a ring's detail)
/// in a column of its own, which a meter and a ring label with.
fn stat_chart(label: &str, value: f64, beside: (&str, Option<&str>), tone: Tone) -> Chart {
    let mut columns = vec!["value".to_string()];
    let mut row = ListValue::from_values([value]);
    let channel = |field: &str, kind: chart::FieldType| chart::Channel {
        field: field.to_string(),
        r#type: kind.into(),
        ..Default::default()
    };
    let mut encoding = chart::Encoding {
        y: Some(channel("value", chart::FieldType::FIELD_TYPE_QUANTITATIVE)).into(),
        ..Default::default()
    };
    if let (name, Some(beside)) = beside {
        columns.push(name.to_string());
        row.values.push(beside.into());
        if name != "delta" {
            encoding.label = Some(channel(name, chart::FieldType::FIELD_TYPE_NOMINAL)).into();
        }
    }
    Chart {
        r#type: chart::Type::TYPE_STAT.into(),
        title: label.to_string(),
        data: Some(chart::Data { columns, rows: vec![row], ..Default::default() }).into(),
        encoding: Some(encoding).into(),
        tone: tone.into(),
        ..Default::default()
    }
}

/// A table: its columns, and its rows, each by an id and its cells (text or numbers).
pub fn table(name: &str, columns: &[&str], rows: Vec<(String, Vec<serde_json::Value>)>) -> Block {
    let data = chart::Data {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        row_ids: rows.iter().map(|(id, _)| id.clone()).collect(),
        rows: rows
            .iter()
            .map(|(_, cells)| ListValue {
                values: cells.iter().map(crate::convert::to_proto).collect(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    let chart = Chart { r#type: chart::Type::TYPE_TABLE.into(), data: Some(data).into(), ..Default::default() };
    named(name, Family::Chart(Box::new(chart)))
}

/// One row of a list: an id, a title, and anything else by setting its fields.
pub fn item(id: &str, title: &str) -> list::Item {
    list::Item { id: id.to_string(), title: title.to_string(), ..Default::default() }
}

/// A list of rows, cards, lines or a timeline.
pub fn list(name: &str, style: list::Style, items: Vec<list::Item>) -> Block {
    named(name, Family::List(Box::new(List { style: style.into(), items, ..Default::default() })))
}

/// A stack of blocks, one above another.
pub fn stack(name: &str, children: Vec<Block>) -> Block {
    let stack = layout::Stack { children, ..Default::default() };
    named(
        name,
        Family::Layout(Box::new(Layout { kind: Some(layout::Kind::Stack(Box::new(stack))), ..Default::default() })),
    )
}

/// A stack of blocks side by side.
pub fn row(name: &str, children: Vec<Block>) -> Block {
    let stack = layout::Stack { children, horizontal: true, ..Default::default() };
    named(
        name,
        Family::Layout(Box::new(Layout { kind: Some(layout::Kind::Stack(Box::new(stack))), ..Default::default() })),
    )
}

/// Tabs: each by an id, unique in the pane, a title and its blocks.
pub fn tabs(name: &str, tabs: Vec<(&str, &str, Vec<Block>)>) -> Block {
    let tabs = tabs
        .into_iter()
        .map(|(id, title, children)| layout::tabs::Tab {
            id: id.to_string(),
            title: title.to_string(),
            children,
            ..Default::default()
        })
        .collect();
    let tabs = layout::Tabs { tabs, ..Default::default() };
    named(
        name,
        Family::Layout(Box::new(Layout { kind: Some(layout::Kind::Tabs(Box::new(tabs))), ..Default::default() })),
    )
}

/// A button: a tap on it sends an input whose `block` is the actions block's name and `value` its id.
pub fn button(id: &str, title: &str) -> input::actions::Button {
    input::actions::Button { id: id.to_string(), title: title.to_string(), ..Default::default() }
}

/// A row of buttons (at most 6).
pub fn actions(name: &str, buttons: Vec<input::actions::Button>) -> Block {
    let actions = input::Actions { buttons, ..Default::default() };
    named(
        name,
        Family::Input(Box::new(Input { kind: Some(input::Kind::Actions(Box::new(actions))), ..Default::default() })),
    )
}

/// Source code.
pub fn code(name: &str, text: &str, language: Option<&str>) -> Block {
    let source =
        crate::v1::code::Source { text: text.to_string(), language: language.map(String::from), ..Default::default() };
    named(
        name,
        Family::Code(Box::new(Code {
            kind: Some(crate::v1::code::Kind::Source(Box::new(source))),
            ..Default::default()
        })),
    )
}

/// Nothing to show yet, and why.
pub fn empty(name: &str, title: &str, note: Option<&str>) -> Block {
    named(
        name,
        Family::Empty(Box::new(Empty { title: title.to_string(), note: note.map(String::from), ..Default::default() })),
    )
}
