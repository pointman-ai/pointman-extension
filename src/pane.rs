//! Panes (Pointman's docs/dev/panes.md): an extension answers `pane.open` for one of its manifest's
//! `[[panes]]` with the first content, then keeps it current with `pane.set` and `pane.edit`. The
//! twin of the Python SDK's `Pane`, message for message.
//!
//! ```no_run
//! use pointman_extension::v1::{block::Family, Block, Text};
//! use pointman_extension::{protocol::PaneOpenResponse, Extension, Pane, Refused};
//! use pointman_extension::v1::PaneInput;
//! use std::sync::atomic::{AtomicU64, Ordering};
//!
//! fn text(name: &str, markdown: String) -> Block {
//!     Block {
//!         name: Some(name.into()),
//!         family: Some(Family::Text(Box::new(Text { markdown, ..Default::default() }))),
//!         ..Default::default()
//!     }
//! }
//!
//! # fn main() -> anyhow::Result<()> {
//! let mut ext = Extension::new()?;
//! ext.pane("counter", |pane: &Pane| {
//!     let n = AtomicU64::new(0);
//!     pane.on_input(move |pane: &Pane, event: &PaneInput| -> anyhow::Result<()> {
//!         if event.str("block") != Some("plus") {
//!             return Err(Refused::new("only + works").into());
//!         }
//!         let n = n.fetch_add(1, Ordering::SeqCst) + 1;
//!         pane.show(vec![text("count", format!("Taps: {n}"))]); // an edit: replace "count"
//!         Ok(())
//!     });
//!     Ok(PaneOpenResponse::blocks(vec![text("count", "Taps: 0".into())]).titled("Counter"))
//! });
//! ext.run()
//! # }
//! ```
//!
//! Each pane's open, inputs, context and close run in order on a thread of its own. To keep a pane
//! current, [`Pane::show`] (or [`Pane::show_board`]) takes all of its content each time and sends only
//! what changed, as edits by block name and item id ([`edits::diff`]), so the apps animate rows and numbers
//! in place.

use std::fmt;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::bail;
use buffa_types::google::protobuf::Struct;
use serde_json::{json, Value};

use crate::protocol::{
    pane_open_response::{First, Page},
    PaneCloseRequest, PaneClosedRequest, PaneContextRequest, PaneEditRequest, PaneInputRequest, PaneInputResponse,
    PaneOpenRequest, PaneOpenResponse, PaneSetRequest, PaneTitleRequest, PaneViewer,
};
use crate::v1::{Block, PaneContent, PaneContext, PaneEdit, PaneFormat, PaneId, PaneInput};
use crate::{caused_by, convert, edits, from_params, guarded, lock, read, spawn, text_or, to_json, write, Handle, Notification, Outcome};

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

impl PaneInput {
    /// An input of this kind (`tap`, `move`…) with these fields (a JSON object).
    pub fn new(kind: &str, fields: Value) -> Self {
        PaneInput { kind: kind.to_string(), fields: convert::to_struct(&fields).into(), ..Default::default() }
    }

    /// Its fields, as a JSON object: `event.json()["block"]`.
    pub fn json(&self) -> Value {
        convert::from_struct(&self.fields)
    }

    /// One of its fields, if it's a string (`block`, `item`, `to`).
    pub fn str(&self, field: &str) -> Option<&str> {
        self.fields.as_option()?.fields.get(field)?.as_str()
    }
}

impl PaneOpenResponse {
    /// A blocks pane's first content.
    pub fn blocks(blocks: Vec<Block>) -> Self {
        PaneOpenResponse::content(PaneContent { blocks, ..Default::default() })
    }

    /// A board pane's first content: its project, states and items as the extension's tracker gives them.
    pub fn board(board: &Value) -> Self {
        PaneOpenResponse::content(PaneContent { board: convert::to_struct(board).into(), ..Default::default() })
    }

    /// A page pane's first file, in the extension's package.
    pub fn page(path: &str) -> Self {
        let page = Page { path: path.to_string(), ..Default::default() };
        PaneOpenResponse { first: Some(First::Page(Box::new(page))), ..Default::default() }
    }

    fn content(content: PaneContent) -> Self {
        PaneOpenResponse { first: Some(First::Content(Box::new(content))), ..Default::default() }
    }

    /// With a title; unset, an earlier title, else the manifest's.
    pub fn titled(mut self, title: &str) -> Self {
        self.title = Some(title.to_string());
        self
    }
}

pub(crate) type OpenFn = Arc<dyn Fn(&Pane) -> anyhow::Result<PaneOpenResponse> + Send + Sync>;
type InputFn = Arc<dyn Fn(&Pane, &PaneInput) -> anyhow::Result<()> + Send + Sync>;
type ContextFn = Arc<dyn Fn(&Pane, &PaneContext) -> anyhow::Result<()> + Send + Sync>;
type CloseFn = Arc<dyn Fn(&Pane, &str) -> anyhow::Result<()> + Send + Sync>;

enum Message {
    Input(PaneInput, Option<Value>),
    Context(PaneContext),
    Close(String),
}

/// What the apps have, as far as [`Pane::show`] knows: nothing sent yet, the content after what was
/// sent, or unknown after edits it couldn't follow (the next show sends all of it).
enum Last {
    Unset,
    Known(PaneContent),
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
    viewer: PaneViewer,
    context: RwLock<PaneContext>,
    gate: Mutex<Gate>,
    last: Mutex<Last>,
    input: Mutex<Option<InputFn>>,
    on_context: Mutex<Vec<ContextFn>>,
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

    fn pane_id(&self) -> buffa::MessageField<PaneId, buffa::Inline<PaneId>> {
        PaneId { value: self.0.id.clone(), ..Default::default() }.into()
    }

    /// Which of the manifest's `[[panes]]` it is.
    pub fn open(&self) -> &str {
        &self.0.open
    }

    /// What to show: `{"url": …}`, `{"item": "MB-286"}` (the extension's own fields).
    pub fn target(&self) -> &Value {
        &self.0.target
    }

    /// Who is looking.
    pub fn viewer(&self) -> &PaneViewer {
        &self.0.viewer
    }

    /// What the person is looking at now: a thread, item, agent, post or file.
    pub fn context(&self) -> PaneContext {
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

    /// All of a `blocks` pane's content.
    pub fn set(&self, blocks: Vec<Block>) {
        self.set_content(PaneContent { blocks, ..Default::default() });
    }

    /// All of a `board` pane's content.
    pub fn set_board(&self, board: &Value) {
        self.set_content(PaneContent { board: convert::to_struct(board).into(), ..Default::default() });
    }

    fn set_content(&self, content: PaneContent) {
        let mut last = lock(&self.0.last);
        self.notify(&PaneSetRequest { pane: self.pane_id(), content: content.clone().into(), ..Default::default() });
        *last = Last::Known(content);
    }

    /// All of a `blocks` pane's content, as it is now: only what changed since the last goes out, as
    /// edits by block name and item id ([`edits::diff`]), or all of it when the edits can't say it. Nothing when
    /// nothing did. Give each block a `name`, unique in the pane.
    pub fn show(&self, blocks: Vec<Block>) {
        let mut last = lock(&self.0.last);
        let new = PaneContent { blocks, ..Default::default() };
        let edits = match &*last {
            Last::Known(old) if *old == new => return,
            Last::Known(old) => edits::diff(&old.blocks, &new.blocks),
            Last::Unset | Last::Unknown => None,
        };
        self.send_diff(&mut last, new, edits);
    }

    /// All of a `board` pane's content, as it is now: changed items go out as edits by id
    /// ([`edits::board_diff`]).
    pub fn show_board(&self, board: &Value) {
        let mut last = lock(&self.0.last);
        let new = PaneContent { board: convert::to_struct(board).into(), ..Default::default() };
        let edits = match &*last {
            Last::Known(old) if *old == new => return,
            Last::Known(old) => edits::board_diff(&convert::from_struct(old.board.as_option().unwrap_or(&Struct::default())), board),
            Last::Unset | Last::Unknown => None,
        };
        self.send_diff(&mut last, new, edits);
    }

    fn send_diff(&self, last: &mut Last, new: PaneContent, edits: Option<Vec<PaneEdit>>) {
        match edits {
            Some(edits) if edits.is_empty() => {} // only what's the person's changed (the open tab)
            Some(edits) => self.notify(&PaneEditRequest { pane: self.pane_id(), edits, ..Default::default() }),
            None => self.notify(&PaneSetRequest { pane: self.pane_id(), content: new.clone().into(), ..Default::default() }),
        }
        *last = Last::Known(new);
    }

    /// Edits to its content, as they are (`edits::replace`, `put_items`…): applied in order, all or none, by the
    /// node. What [`show`](Self::show) compares with follows them, so the next show sends only what changed after.
    pub fn edit(&self, edits: Vec<PaneEdit>) {
        if edits.is_empty() {
            return;
        }
        let mut last = lock(&self.0.last);
        self.notify(&PaneEditRequest { pane: self.pane_id(), edits: edits.clone(), ..Default::default() });
        *last = match std::mem::replace(&mut *last, Last::Unknown) {
            Last::Known(content) => edits::apply(&content, &edits).map_or(Last::Unknown, Last::Known),
            _ => Last::Unknown,
        };
    }

    /// Replace one block, by its name.
    pub fn replace(&self, block: Block) {
        self.edit(vec![edits::replace(block, false)]);
    }

    /// Put one board item (a JSON object with its `id`): replaced where it is, or added.
    pub fn put_board_item(&self, item: &Value) {
        self.edit(vec![edits::put_board_items(&[item.clone()])]);
    }

    pub fn set_title(&self, title: &str) {
        self.notify(&PaneTitleRequest { pane: self.pane_id(), title: title.to_string(), ..Default::default() });
    }

    /// Close it from the extension's side: what it showed is gone, say.
    pub fn close(&self, reason: &str) {
        if self.drop_it(reason) {
            self.0.handle.notify(&PaneClosedRequest { pane: self.pane_id(), reason: reason.to_string(), ..Default::default() });
        }
    }

    /// `f(pane, input)` for each tap, submit, select, scroll_end, resize or board gesture (`input.kind`, and its
    /// fields: [`PaneInput::str`]). Change the pane to answer it; return a [`Refused`] (or any error) to turn it
    /// down. A pane with no handler refuses every input.
    pub fn on_input<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, &PaneInput) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        *lock(&self.0.input) = Some(Arc::new(move |pane: &Pane, event: &PaneInput| f(pane, event).into_result()));
        self
    }

    /// `f(pane, context)` when what the person is looking at changes.
    pub fn on_context<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, &PaneContext) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        lock(&self.0.on_context).push(Arc::new(move |pane: &Pane, context: &PaneContext| f(pane, context).into_result()));
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

    fn notify<N: Notification>(&self, n: &N) {
        let msg = json!({"jsonrpc": "2.0", "method": N::METHOD, "params": to_json(n)});
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

// ------------------------------------------------------------------ the node's side

fn error(h: &Handle, rid: Option<Value>, message: String) {
    if let Some(rid) = rid {
        h.send(&json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": message}}));
    }
}

/// One of the node's `pane.*` messages. `opener` is the open handler and the manifest's format
/// for a `pane.open`.
pub(crate) fn message(h: &Handle, method: &str, params: Value, rid: Option<Value>, opener: Option<(OpenFn, Option<String>)>) {
    if method == "pane.open" {
        let request: PaneOpenRequest = match from_params(params, method) {
            Ok(request) => request,
            Err(e) => return error(h, rid, format!("{e:#}")),
        };
        let pid = request.pane.as_option().map(|p| p.value.clone()).unwrap_or_default();
        let open = request.open.clone();
        let mut panes = lock(&h.0.panes);
        let why = match &opener {
            None => Some(format!("{open} isn't a pane of {}", h.id())),
            Some(_) if pid.is_empty() => Some("pane.open has no pane".to_string()),
            Some(_) if panes.contains_key(&pid) => Some(format!("pane '{pid}' is already open")),
            Some(_) => None,
        };
        if let Some(why) = &why {
            error(h, rid.clone(), why.clone());
        }
        let Some((opener, format)) = opener.filter(|_| why.is_none()) else { return };
        let (tx, rx) = mpsc::channel();
        let pane = Pane(Arc::new(Inner {
            id: pid.clone(),
            open,
            target: convert::from_struct(&request.target),
            viewer: request.viewer.as_option().cloned().unwrap_or_default(),
            context: RwLock::new(request.context.as_option().cloned().unwrap_or_default()),
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
    let pid = match method {
        "pane.input" => from_params::<PaneInputRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option()),
        "pane.context" => from_params::<PaneContextRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option()),
        _ => from_params::<PaneCloseRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option()),
    }
    .map(|p| p.value)
    .unwrap_or_default();
    let pane = lock(&h.0.panes).get(&pid).cloned();
    let Some(pane) = pane else {
        return error(h, rid, format!("no pane {pid} is open"));
    };
    match method {
        "pane.close" => {
            let reason = from_params::<PaneCloseRequest>(params, method).map(|r| r.reason).unwrap_or_default();
            pane.drop_it(if reason.is_empty() { "closed" } else { &reason });
        }
        "pane.context" => {
            let context = from_params::<PaneContextRequest>(params, method).ok().and_then(|r| r.context.into_option()).unwrap_or_default();
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Context(context));
            }
        }
        _ => {
            let event = from_params::<PaneInputRequest>(params, method).ok().and_then(|r| r.event.into_option()).unwrap_or_default();
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
                *last = match &answer.first {
                    Some(First::Content(content)) => Last::Known((**content).clone()),
                    _ => Last::Unknown,
                };
            }
            drop(last);
            // the answer first, then what it sent while opening
            let mut gate = lock(&pane.0.gate);
            if let Some(rid) = rid {
                h.reply(rid, Ok(to_json(&answer)));
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
                    Ok(()) => json!({"jsonrpc": "2.0", "id": rid, "result": to_json(PaneInputResponse::default())}),
                    Err(e) => {
                        if !caused_by::<Refused>(&e) {
                            h.log(format!("pane {} input {} {}: {e:?}", pane.0.id, event.kind, event.json()));
                        }
                        json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": text_or(&e, "refused")}})
                    }
                });
            }
        }
    }
}

/// The format a manifest's `format` names.
pub fn format_named(name: &str) -> Option<PaneFormat> {
    Some(match name {
        "blocks" => PaneFormat::PANE_FORMAT_BLOCKS,
        "board" => PaneFormat::PANE_FORMAT_BOARD,
        "page" => PaneFormat::PANE_FORMAT_PAGE,
        "video" => PaneFormat::PANE_FORMAT_VIDEO,
        "call" => PaneFormat::PANE_FORMAT_CALL,
        _ => return None,
    })
}

/// The open handler's answer, with the manifest's format, if it has what that format opens with.
fn opened(pane: &Pane, mut answer: PaneOpenResponse, format: Option<String>) -> anyhow::Result<PaneOpenResponse> {
    let given = answer.format.as_known().filter(|f| *f != PaneFormat::PANE_FORMAT_UNSPECIFIED);
    let Some(format) = given.or_else(|| format.as_deref().and_then(format_named)) else {
        bail!("the {} pane has no format (blocks, board, page, video or call) in extension.toml", pane.0.open);
    };
    answer.format = format.into();
    let (ok, what) = match (format, &answer.first) {
        (PaneFormat::PANE_FORMAT_BLOCKS, Some(First::Content(c))) => (c.board.is_unset(), "its content, as blocks"),
        (PaneFormat::PANE_FORMAT_BOARD, Some(First::Content(c))) => (c.board.is_set(), "its content, as a board"),
        (PaneFormat::PANE_FORMAT_PAGE, Some(First::Page(p))) => (!p.path.is_empty(), "its page, with a path"),
        (PaneFormat::PANE_FORMAT_VIDEO, Some(First::Video(_))) => (true, ""),
        (PaneFormat::PANE_FORMAT_CALL, Some(First::Call(_))) => (true, ""),
        (PaneFormat::PANE_FORMAT_BLOCKS, _) => (false, "its content, as blocks"),
        (PaneFormat::PANE_FORMAT_BOARD, _) => (false, "its content, as a board"),
        (PaneFormat::PANE_FORMAT_PAGE, _) => (false, "its page, with a path"),
        (PaneFormat::PANE_FORMAT_VIDEO, _) => (false, "its video"),
        _ => (false, "its call"),
    };
    if !ok {
        bail!("a {} pane opens with {what}", pane_format_word(format));
    }
    Ok(answer)
}

pub(crate) fn pane_format_word(format: PaneFormat) -> &'static str {
    match format {
        PaneFormat::PANE_FORMAT_BLOCKS => "blocks",
        PaneFormat::PANE_FORMAT_BOARD => "board",
        PaneFormat::PANE_FORMAT_PAGE => "page",
        PaneFormat::PANE_FORMAT_VIDEO => "video",
        PaneFormat::PANE_FORMAT_CALL => "call",
        _ => "unspecified",
    }
}
