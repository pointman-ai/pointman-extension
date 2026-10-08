//! Panes (Pointman's docs/dev/panes.md): an extension answers `pane.open` for one of its manifest's
//! `[[panes]]` with the first content, then keeps it current with `pane.set` and `pane.patch`. The
//! twin of the Python SDK's `Pane`, message for message.
//!
//! ```no_run
//! use pointman_extension::{Extension, Pane, Refused};
//! use serde_json::{json, Value};
//! use std::sync::atomic::{AtomicU64, Ordering};
//!
//! # fn main() -> anyhow::Result<()> {
//! let mut ext = Extension::new()?;
//! ext.pane("counter", |pane: &Pane| {
//!     let n = AtomicU64::new(0);
//!     pane.on_input(move |pane: &Pane, event: &Value| -> anyhow::Result<()> {
//!         if event["block"] != "plus" {
//!             return Err(Refused::new("only + works").into());
//!         }
//!         let n = n.fetch_add(1, Ordering::SeqCst) + 1;
//!         pane.update("count", json!({"value": n})); // a patch: /b/count/value
//!         Ok(())
//!     });
//!     Ok(json!({"title": "Counter", "blocks": [
//!         {"type": "stat", "id": "count", "label": "Taps", "value": 0},
//!         {"type": "actions", "id": "plus", "buttons": [{"id": "plus", "title": "+"}]}]}))
//! });
//! ext.run()
//! # }
//! ```
//!
//! Each pane's open, inputs, context and close run in order on a thread of its own. To keep a pane
//! current, [`Pane::show`] (or [`Pane::show_board`]) takes all of its content each time and sends only
//! what changed, as patches by id, so the apps animate rows and numbers in place.

use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::bail;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::{caused_by, guarded, id_text, lock, read, spawn, text_or, to_json, write, Handle, Outcome};

/// Turn down a pane's input: the app undoes what it showed early (a card snaps back) and shows
/// why. `return Err(Refused::new("Linear decides when parents close").into())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(String);

impl Refused {
    pub fn new(why: impl Into<String>) -> Self {
        Refused(why.into())
    }
}

impl fmt::Display for Refused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A patch path from its parts, each escaped (RFC 6901): `pointer(["b", "files", "2"])` is
/// `/b/files/2`.
pub fn pointer<I, T>(parts: I) -> String
where
    I: IntoIterator<Item = T>,
    T: fmt::Display,
{
    parts.into_iter().map(|part| format!("/{}", part.to_string().replace('~', "~0").replace('/', "~1"))).collect()
}

pub(crate) type OpenFn = Arc<dyn Fn(&Pane) -> anyhow::Result<Value> + Send + Sync>;
type InputFn = Arc<dyn Fn(&Pane, &Value) -> anyhow::Result<()> + Send + Sync>;
type CloseFn = Arc<dyn Fn(&Pane, &str) -> anyhow::Result<()> + Send + Sync>;

enum Message {
    Input(Value, Option<Value>),
    Context(Value),
    Close(String),
}

/// What the apps have, as far as [`Pane::show`] knows: nothing sent yet, the content after what was
/// sent, or unknown after a hand-made patch (the next show sends all of it).
enum Last {
    Unset,
    Known(Value),
    Unknown,
}

/// Whether it's closed, and what it sent before its open was answered (sent right after).
struct Gate {
    closed: Option<String>,
    held: Option<Vec<Value>>,
}

struct Inner {
    id: String,
    open: String,
    target: Value,
    viewer: Value,
    context: RwLock<Value>,
    gate: Mutex<Gate>,
    last: Mutex<Last>,
    input: Mutex<Option<InputFn>>,
    on_context: Mutex<Vec<InputFn>>,
    on_close: Mutex<Vec<CloseFn>>,
    queue: Mutex<Option<Sender<Message>>>,
    handle: Handle,
}

/// One open pane: what opened it, and the line to the person's apps showing it. Given to the
/// extension's [`Extension::pane`](crate::Extension::pane) handler; cheap to clone, and usable from
/// any thread.
#[derive(Clone)]
pub struct Pane(Arc<Inner>);

impl fmt::Debug for Pane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pane").field("id", &self.0.id).field("open", &self.0.open).finish_non_exhaustive()
    }
}

impl Pane {
    /// Its id (`pane_12`), core's.
    pub fn id(&self) -> &str {
        &self.0.id
    }

    /// Which of the manifest's `[[panes]]` it is.
    pub fn open(&self) -> &str {
        &self.0.open
    }

    /// What to show: `{"url": …}`, `{"item": "MB-286"}`.
    pub fn target(&self) -> &Value {
        &self.0.target
    }

    /// Who is looking: `{person, device}`.
    pub fn viewer(&self) -> &Value {
        &self.0.viewer
    }

    /// What the person is looking at now: ids of a `thread`, `item`, `agent`, `post` or `file`.
    pub fn context(&self) -> Value {
        read(&self.0.context).clone()
    }

    /// Why it closed, once it has.
    pub fn closed(&self) -> Option<String> {
        lock(&self.0.gate).closed.clone()
    }

    /// Its open is answered, and it hasn't closed.
    pub fn is_open(&self) -> bool {
        let gate = lock(&self.0.gate);
        gate.held.is_none() && gate.closed.is_none()
    }

    /// The extension's [`Handle`].
    pub fn handle(&self) -> &Handle {
        &self.0.handle
    }

    /// All of a `blocks` pane's content: blocks, version 2.
    pub fn set(&self, blocks: impl Serialize) {
        let blocks = to_json(blocks);
        let mut last = lock(&self.0.last);
        self.notify("pane.set", "blocks", blocks.clone());
        *last = Last::Known(blocks);
    }

    /// All of a `board` pane's content.
    pub fn set_board(&self, board: impl Serialize) {
        let board = to_json(board);
        let mut last = lock(&self.0.last);
        self.notify("pane.set", "board", board.clone());
        *last = Last::Known(board);
    }

    /// All of a `blocks` pane's content, as it is now: only what changed since the last goes out, as
    /// patches by id ([`changes`]), or all of it when the blocks themselves changed. Nothing when
    /// nothing did.
    pub fn show(&self, blocks: impl Serialize) {
        self.show_as("blocks", to_json(blocks));
    }

    /// All of a `board` pane's content, as it is now: changed items go out as patches by id
    /// ([`board_changes`]).
    pub fn show_board(&self, board: impl Serialize) {
        self.show_as("board", to_json(board));
    }

    fn show_as(&self, key: &str, new: Value) {
        let mut last = lock(&self.0.last);
        let ops = match &*last {
            Last::Known(old) if *old == new => return,
            Last::Known(old) if key == "board" => Some(board_changes(old, &new)),
            Last::Known(old) => changes(old, &new),
            Last::Unset | Last::Unknown => None,
        };
        match ops {
            Some(ops) if ops.is_empty() => {} // only what's the person's changed (the open tab)
            Some(ops) => self.notify("pane.patch", "patch", Value::Array(ops)),
            None => self.notify("pane.set", key, new.clone()),
        }
        *last = Last::Known(new);
    }

    /// RFC 6902 operations on its content (add, replace and remove). Paths go through ids,
    /// `/b/<block>/…` or `/i/<item>/…` ([`pointer`]), so a patch still lands when a list above it
    /// grows.
    pub fn patch(&self, ops: impl Serialize) {
        let ops = to_json(ops);
        if ops.as_array().is_some_and(|ops| !ops.is_empty()) {
            let mut last = lock(&self.0.last);
            self.notify("pane.patch", "patch", ops);
            *last = Last::Unknown;
        }
    }

    /// Set some of one block's fields, there before or not: `pane.update("count", json!({"value": 3}))`.
    pub fn update(&self, block: &str, fields: Value) {
        self.set_fields("b", block, fields);
    }

    /// Set some of one board item's fields: `pane.update_item("lin_9f2", json!({"state": "Done"}))`.
    pub fn update_item(&self, item: &str, fields: Value) {
        self.set_fields("i", item, fields);
    }

    /// A patch of field sets, kept in what [`show`](Self::show) compares with, so the next show sends
    /// only what changed after it.
    fn set_fields(&self, kind: &str, id: &str, fields: Value) {
        let ops = set_ops(kind, id, &fields);
        if ops.is_empty() {
            return;
        }
        let mut last = lock(&self.0.last);
        self.notify("pane.patch", "patch", Value::Array(ops));
        let kept = match (&mut *last, fields) {
            (Last::Known(content), Value::Object(fields)) => {
                let target = if kind == "b" { find_block_mut(content, id) } else { find_item_mut(content, id) };
                target.and_then(Value::as_object_mut).map(|target| target.extend(fields)).is_some()
            }
            _ => false,
        };
        if !kept {
            *last = Last::Unknown;
        }
    }

    pub fn set_title(&self, title: &str) {
        self.notify("pane.title", "title", json!(title));
    }

    /// Close it from the extension's side: what it showed is gone, say.
    pub fn close(&self, reason: &str) {
        if self.drop_it(reason) {
            self.0.handle.notify("pane.closed", json!({"pane": self.0.id, "reason": reason}));
        }
    }

    /// `f(pane, event)` for each tap, submit, select, scroll_end, resize or board gesture
    /// (`{kind, block, value}` or `{kind, item, to}`). Change the pane to answer it; return a
    /// [`Refused`] (or any error) to turn it down. A pane with no handler refuses every input.
    pub fn on_input<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, &Value) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        *lock(&self.0.input) = Some(Arc::new(move |pane: &Pane, event: &Value| f(pane, event).into_result()));
        self
    }

    /// `f(pane, context)` when what the person is looking at changes.
    pub fn on_context<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, &Value) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        lock(&self.0.on_context).push(Arc::new(move |pane: &Pane, context: &Value| f(pane, context).into_result()));
        self
    }

    /// `f(pane, reason)` once it's closed, by either side: stop what feeds it.
    pub fn on_close<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, &str) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        lock(&self.0.on_close).push(Arc::new(move |pane: &Pane, reason: &str| f(pane, reason).into_result()));
        self
    }

    fn notify(&self, method: &str, key: &str, value: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": {"pane": self.0.id, key: value}});
        let mut gate = lock(&self.0.gate);
        if gate.closed.is_some() {
            return;
        }
        match gate.held.as_mut() {
            Some(held) => held.push(msg),
            None => self.0.handle.send(&msg),
        }
    }

    /// It's closed: no more sending, and its on_close handlers run after what's queued. False if it
    /// already was.
    fn drop_it(&self, reason: &str) -> bool {
        {
            let mut gate = lock(&self.0.gate);
            if gate.closed.is_some() {
                return false;
            }
            gate.closed = Some(reason.to_string());
        }
        let mut panes = lock(&self.0.handle.0.panes);
        if panes.get(&self.0.id).is_some_and(|p| Arc::ptr_eq(&p.0, &self.0)) {
            panes.remove(&self.0.id);
        }
        drop(panes);
        if let Some(tx) = lock(&self.0.queue).take() {
            let _ = tx.send(Message::Close(reason.to_string()));
        }
        true
    }
}

fn set_ops(kind: &str, id: &str, fields: &Value) -> Vec<Value> {
    match fields {
        Value::Object(fields) => fields
            .iter()
            .map(|(field, value)| json!({"op": "add", "path": pointer([kind, id, field.as_str()]), "value": value}))
            .collect(),
        _ => Vec::new(),
    }
}

/// The block with this id, anywhere in a `blocks` content (in `children` and `tabs[].children`).
fn find_block_mut<'a>(v: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    let at = block_at(v, id, String::new())?;
    v.pointer_mut(&at)
}

/// Where the block with this id is, as a JSON Pointer from the top of the content.
fn block_at(v: &Value, id: &str, at: String) -> Option<String> {
    for (i, block) in v.as_array()?.iter().enumerate() {
        let here = format!("{at}/{i}");
        if block["id"] == id {
            return Some(here);
        }
        if let Some(found) = block_at(&block["children"], id, format!("{here}/children")) {
            return Some(found);
        }
        for (t, tab) in block["tabs"].as_array().into_iter().flatten().enumerate() {
            if let Some(found) = block_at(&tab["children"], id, format!("{here}/tabs/{t}/children")) {
                return Some(found);
            }
        }
    }
    None
}

fn find_item_mut<'a>(board: &'a mut Value, id: &str) -> Option<&'a mut Value> {
    board.get_mut("items")?.as_array_mut()?.iter_mut().find(|item| item["id"] == id)
}

// ------------------------------------------------------------------ the node's side

/// One of the node's `pane.*` messages. `opener` is the open handler and the manifest's format
/// for a `pane.open`.
pub(crate) fn message(
    h: &Handle,
    method: &str,
    p: &Value,
    rid: Option<Value>,
    opener: Option<(OpenFn, Option<String>)>,
) {
    let pid = p.get("pane").filter(|id| !id.is_null()).map(id_text).unwrap_or_default();
    if method == "pane.open" {
        let open = p["open"].as_str().unwrap_or_default().to_string();
        let mut panes = lock(&h.0.panes);
        let why = match &opener {
            None => Some(format!("{open} isn't a pane of {}", h.id())),
            Some(_) if pid.is_empty() => Some("pane.open has no pane".to_string()),
            Some(_) if panes.contains_key(&pid) => Some(format!("pane '{pid}' is already open")),
            Some(_) => None,
        };
        if let (Some(why), Some(rid)) = (&why, &rid) {
            h.send(&json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": why}}));
        }
        let Some((opener, format)) = opener.filter(|_| why.is_none()) else { return };
        let (tx, rx) = mpsc::channel();
        let field = |key: &str| p.get(key).filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!({}));
        let pane = Pane(Arc::new(Inner {
            id: pid.clone(),
            open,
            target: field("target"),
            viewer: field("viewer"),
            context: RwLock::new(field("context")),
            gate: Mutex::new(Gate { closed: None, held: Some(Vec::new()) }),
            last: Mutex::new(Last::Unset),
            input: Mutex::new(None),
            on_context: Mutex::default(),
            on_close: Mutex::default(),
            queue: Mutex::new(Some(tx)),
            handle: h.clone(),
        }));
        panes.insert(pid.clone(), pane.clone());
        drop(panes);
        spawn(format!("pane-{pid}"), move || run(pane, rx, opener, format, rid));
        return;
    }
    let pane = lock(&h.0.panes).get(&pid).cloned();
    let Some(pane) = pane else {
        if let Some(rid) = rid {
            let why = format!("no pane {pid} is open");
            h.send(&json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": why}}));
        }
        return;
    };
    match method {
        "pane.close" => {
            pane.drop_it(p["reason"].as_str().filter(|r| !r.is_empty()).unwrap_or("closed"));
        }
        "pane.context" => {
            let context = p.get("context").filter(|c| !c.is_null()).cloned().unwrap_or_else(|| json!({}));
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Context(context));
            }
        }
        _ => {
            let event = p.get("event").filter(|e| !e.is_null()).cloned().unwrap_or_else(|| json!({}));
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Input(event, rid));
            }
        }
    }
}

fn run(pane: Pane, rx: Receiver<Message>, opener: OpenFn, format: Option<String>, rid: Option<Value>) {
    let h = pane.0.handle.clone();
    match guarded(|| opener(&pane)).and_then(|answer| opened(&pane, answer, format)) {
        Err(e) => {
            h.log(format!("pane {} ({}) didn't open: {e:?}", pane.0.id, pane.0.open));
            pane.drop_it("failed");
            if let Some(rid) = rid {
                h.reply(rid, Err(e));
            }
            return;
        }
        Ok(answer) => {
            // what the apps have now, unless it sent more while opening (which goes after the answer)
            let mut last = lock(&pane.0.last);
            if matches!(*last, Last::Unset) {
                let content = answer.get("blocks").or_else(|| answer.get("board")).cloned();
                *last = content.map_or(Last::Unknown, Last::Known);
            }
            drop(last);
            // the answer first, then what it sent while opening
            let mut gate = lock(&pane.0.gate);
            if let Some(rid) = rid {
                h.reply(rid, Ok(answer));
            }
            for msg in gate.held.take().unwrap_or_default() {
                h.send(&msg);
            }
        }
    }
    for msg in rx {
        match msg {
            Message::Close(reason) => {
                let handlers = lock(&pane.0.on_close).clone();
                for f in handlers {
                    if let Err(e) = guarded(|| f(&pane, &reason)) {
                        h.log(format!("pane {} on_close: {e:?}", pane.0.id));
                    }
                }
                return;
            }
            Message::Context(context) => {
                *write(&pane.0.context) = context.clone();
                let handlers = lock(&pane.0.on_context).clone();
                for f in handlers {
                    if let Err(e) = guarded(|| f(&pane, &context)) {
                        h.log(format!("pane {} on_context: {e:?}", pane.0.id));
                    }
                }
            }
            Message::Input(event, rid) => {
                let f = lock(&pane.0.input).clone();
                let outcome = match f {
                    None => Err(Refused::new("this pane takes no input").into()),
                    Some(f) => guarded(|| f(&pane, &event)),
                };
                let Some(rid) = rid else { continue };
                h.send(&match outcome {
                    Ok(()) => json!({"jsonrpc": "2.0", "id": rid, "result": {}}),
                    Err(e) => {
                        if !caused_by::<Refused>(&e) {
                            h.log(format!("pane {} input {event}: {e:?}", pane.0.id));
                        }
                        json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": text_or(&e, "refused")}})
                    }
                });
            }
        }
    }
}

/// The open handler's answer, with the manifest's format, if it has what that format opens with.
fn opened(pane: &Pane, answer: Value, format: Option<String>) -> anyhow::Result<Value> {
    let Value::Object(given) = answer else {
        bail!("the {} pane's handler must return an object, not {}", pane.0.open, kind_of(&answer));
    };
    let mut answer = Map::new();
    answer.insert("format".into(), format.map_or(Value::Null, Value::String));
    answer.extend(given);
    let format = answer["format"].as_str().unwrap_or_default().to_string();
    let (key, ok, what): (&str, fn(&Value) -> bool, &str) = match format.as_str() {
        "blocks" => ("blocks", Value::is_array, "an array"),
        "board" => ("board", Value::is_object, "an object"),
        "page" => ("path", Value::is_string, "a string"),
        _ => bail!("the {} pane has no format (blocks, board or page) in extension.toml", pane.0.open),
    };
    if !answer.get(key).is_some_and(ok) {
        bail!("a {format} pane opens with its {key}, {what}");
    }
    Ok(Value::Object(answer))
}

/// What changed from one `blocks` content to the next, as patches by id: a block's changed fields are
/// set, and a list that only grew gets its new entries added at the end (new comments slide in), so the
/// apps keep their tab and their scroll. None when the blocks themselves changed (added, gone, moved
/// or of another type): send all of it. Which tab is open is the person's, so `selected` never goes.
pub fn changes(old: &Value, new: &Value) -> Option<Vec<Value>> {
    let mut ops = Vec::new();
    same_blocks(old.as_array()?, new.as_array()?, &mut ops).then_some(ops)
}

fn same_blocks(old: &[Value], new: &[Value], ops: &mut Vec<Value>) -> bool {
    old.len() == new.len()
        && old.iter().zip(new).all(|(a, b)| a["id"] == b["id"] && a["type"] == b["type"] && b["id"].is_string())
        && old.iter().zip(new).all(|(a, b)| same_block(a, b, ops))
}

fn same_block(old: &Value, new: &Value, ops: &mut Vec<Value>) -> bool {
    let (Some(a), Some(b), Some(id)) = (old.as_object(), new.as_object(), new["id"].as_str()) else { return false };
    let none = Vec::new();
    for key in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))) {
        let (before, after) = (a.get(key), b.get(key));
        if before == after || key == "selected" {
            continue;
        }
        let path = pointer(["b", id, key.as_str()]);
        match (key.as_str(), before, after) {
            ("children", Some(Value::Array(x)), Some(Value::Array(y))) => {
                if !same_blocks(x, y, ops) {
                    return false;
                }
            }
            ("tabs", Some(Value::Array(x)), Some(Value::Array(y))) => {
                if x.len() != y.len() || x.iter().zip(y).any(|(s, t)| s["id"] != t["id"]) {
                    return false;
                }
                for (i, (s, t)) in x.iter().zip(y).enumerate() {
                    if s["title"] != t["title"] {
                        let at = pointer(["b", id, "tabs", &i.to_string(), "title"]);
                        ops.push(json!({"op": "add", "path": at, "value": t["title"]}));
                    }
                    let children = |tab: &Value| tab["children"].as_array().unwrap_or(&none).clone();
                    if !same_blocks(&children(s), &children(t), ops) {
                        return false;
                    }
                }
            }
            (_, Some(Value::Array(x)), Some(Value::Array(y))) if y.len() > x.len() && y.starts_with(x) => {
                for item in &y[x.len()..] {
                    ops.push(json!({"op": "add", "path": format!("{path}/-"), "value": item}));
                }
            }
            (_, _, Some(value)) => ops.push(json!({"op": "add", "path": path, "value": value})),
            (_, Some(_), None) => ops.push(json!({"op": "remove", "path": path})),
            (_, None, None) => {}
        }
    }
    true
}

/// What changed from one `board` content to the next: items by id (their changed fields set, new ones
/// added, gone ones removed; the apps order cards by `rank`), and the rest (`project`, `states`,
/// `groups`) whole where it changed.
pub fn board_changes(old: &Value, new: &Value) -> Vec<Value> {
    let mut ops = Vec::new();
    let empty = Map::new();
    let (a, b) = (old.as_object().unwrap_or(&empty), new.as_object().unwrap_or(&empty));
    for key in a.keys().chain(b.keys().filter(|k| !a.contains_key(*k))).filter(|k| *k != "items") {
        match (a.get(key), b.get(key)) {
            (x, Some(y)) if x != Some(y) => ops.push(json!({"op": "add", "path": pointer(["board", key]), "value": y})),
            (Some(_), None) => ops.push(json!({"op": "remove", "path": pointer(["board", key])})),
            _ => {}
        }
    }
    let items = |v: &Value| -> Vec<Value> { v["items"].as_array().cloned().unwrap_or_default() };
    let (was, now) = (items(old), items(new));
    let id = |item: &Value| item["id"].as_str().map(String::from);
    for item in &was {
        if let Some(i) = id(item).filter(|i| !now.iter().any(|n| id(n).as_deref() == Some(i))) {
            ops.push(json!({"op": "remove", "path": pointer(["i", &i])}));
        }
    }
    for item in &now {
        let Some(i) = id(item) else { continue };
        let Some(before) = was.iter().find(|w| id(w).as_deref() == Some(&i)) else {
            ops.push(json!({"op": "add", "path": "/board/items/-", "value": item}));
            continue;
        };
        let (x, y) = (before.as_object().unwrap_or(&empty), item.as_object().unwrap_or(&empty));
        for key in x.keys().chain(y.keys().filter(|k| !x.contains_key(*k))) {
            match (x.get(key), y.get(key)) {
                (p, Some(q)) if p != Some(q) => {
                    ops.push(json!({"op": "add", "path": pointer(["i", &i, key]), "value": q}))
                }
                (Some(_), None) => ops.push(json!({"op": "remove", "path": pointer(["i", &i, key])})),
                _ => {}
            }
        }
    }
    ops
}

fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers_escape_their_parts() {
        assert_eq!(pointer(["b", "files/2", "a~b"]), "/b/files~12/a~0b");
        assert_eq!(pointer(["i", "lin_9f2", "state"]), "/i/lin_9f2/state");
        assert_eq!(pointer::<_, &str>([]), "");
    }

    #[test]
    fn changes_go_by_id_and_lists_that_grew_get_added_to() {
        let old = json!([
            {"type": "heading", "id": "head", "text": "PR 42", "subtitle": "open"},
            {"type": "tabs", "id": "tabs", "selected": "a", "tabs": [
                {"id": "a", "title": "Conversation", "children": [
                    {"type": "timeline", "id": "timeline", "events": [{"id": "e1"}]}]},
                {"id": "b", "title": "Files 1", "children": [{"type": "stat", "id": "n", "value": 1, "tone": "good"}]}]}]);
        let mut new = old.clone();
        new[0]["subtitle"] = json!("merged");
        new[1]["selected"] = json!("b");
        new[1]["tabs"][0]["children"][0]["events"].as_array_mut().unwrap().push(json!({"id": "e2"}));
        new[1]["tabs"][1]["title"] = json!("Files 2");
        new[1]["tabs"][1]["children"][0]["value"] = json!(2);
        new[1]["tabs"][1]["children"][0].as_object_mut().unwrap().remove("tone");
        assert_eq!(
            changes(&old, &new).unwrap(),
            [
                json!({"op": "add", "path": "/b/head/subtitle", "value": "merged"}),
                json!({"op": "add", "path": "/b/timeline/events/-", "value": {"id": "e2"}}),
                json!({"op": "add", "path": "/b/tabs/tabs/1/title", "value": "Files 2"}),
                json!({"op": "remove", "path": "/b/n/tone"}),
                json!({"op": "add", "path": "/b/n/value", "value": 2}),
            ]
        );
        assert_eq!(changes(&old, &old), Some(vec![]));
        // blocks added, gone, moved or of another type: all of it goes
        assert_eq!(changes(&old, &json!([old[1], old[0]])), None);
        assert_eq!(changes(&old, &json!([old[0]])), None);
        let mut retyped = old.clone();
        retyped[0]["type"] = json!("text");
        assert_eq!(changes(&old, &retyped), None);
    }

    #[test]
    fn board_changes_go_by_item() {
        let old = json!({"project": {"key": "ALA"}, "states": [{"name": "Todo"}], "items": [
            {"id": "a", "state": "Todo", "title": "A", "owner": "james"}, {"id": "b", "state": "Todo"}]});
        let new = json!({"project": {"key": "ALA"}, "states": [{"name": "Todo"}, {"name": "Done"}], "items": [
            {"id": "a", "state": "Done", "title": "A"}, {"id": "c", "state": "Todo"}]});
        assert_eq!(
            board_changes(&old, &new),
            [
                json!({"op": "add", "path": "/board/states", "value": [{"name": "Todo"}, {"name": "Done"}]}),
                json!({"op": "remove", "path": "/i/b"}),
                json!({"op": "remove", "path": "/i/a/owner"}),
                json!({"op": "add", "path": "/i/a/state", "value": "Done"}),
                json!({"op": "add", "path": "/board/items/-", "value": {"id": "c", "state": "Todo"}}),
            ]
        );
        assert!(board_changes(&new, &new).is_empty());
    }

    #[test]
    fn update_sets_each_field_by_id() {
        assert_eq!(
            Value::Array(set_ops("b", "count", &json!({"value": 3, "tone": "good"}))),
            json!([{"op": "add", "path": "/b/count/tone", "value": "good"},
                   {"op": "add", "path": "/b/count/value", "value": 3}])
        );
        assert!(set_ops("i", "x", &json!(null)).is_empty());
    }
}
