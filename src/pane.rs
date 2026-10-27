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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::bail;
use buffa_types::google::protobuf::Struct;
use serde_json::{json, Value};

use crate::protocol::{
    pane_audio_request::Sound,
    pane_open_response::{First, Page, Video},
    PaneAudioRequest, PaneCloseRequest, PaneClosedRequest, PaneContextRequest, PaneControlRequest, PaneControlResponse,
    PaneEditRequest, PaneFrameRequest, PaneInputRequest, PaneInputResponse, PaneOpenRequest, PaneOpenResponse,
    PaneSetRequest, PaneTitleRequest, PaneViewer, PaneViewersRequest,
};
use crate::v1::{Block, PaneContent, PaneContext, PaneEdit, PaneFormat, PaneInput, PaneRef};
use crate::{
    caused_by, convert, edits, from_params, guarded, lock, read, spawn, text_or, to_json, write, Handle, Notification,
    Outcome,
};

/// The most a video frame may be once the node has sealed it (docs/dev/video.md): the node drops a
/// bigger one, so [`Pane::frame`] doesn't send it.
pub const VIDEO_FRAME_MAX: usize = 4 << 20;
/// What sealing adds to a frame's JPEG on the wire: the 12-byte nonce, the 20-byte header
/// (captured_ms, seq, width, height) and the 16-byte tag.
pub const SEALING: usize = 12 + 20 + 16;
/// The most sound one `pane.audio` may carry: whole ADTS frames, 64 KiB.
pub const AUDIO_MAX: usize = 64 << 10;

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

    /// A video pane's first answer: its size and rate, and its sound's if it has any.
    pub fn video(video: Video) -> Self {
        PaneOpenResponse { first: Some(First::Video(Box::new(video))), ..Default::default() }
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
type ViewersFn = Arc<dyn Fn(&Pane, u64) -> anyhow::Result<()> + Send + Sync>;
type ControlFn = Arc<dyn Fn(&Pane, Option<&PaneControlRequest>) -> anyhow::Result<()> + Send + Sync>;

enum Message {
    Input(PaneInput, Option<Value>),
    Context(PaneContext),
    Viewers(u64),
    Control(PaneControlRequest, Option<Value>),
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
    on_viewers: Mutex<Vec<ViewersFn>>,
    control: Mutex<Option<ControlFn>>,
    viewers: AtomicU64,
    too_big: AtomicBool,
    too_much_sound: AtomicBool,
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

    fn pane_ref(&self) -> buffa::MessageField<PaneRef, buffa::Inline<PaneRef>> {
        PaneRef { id: self.0.id.clone(), ..Default::default() }.into()
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
        self.notify(&PaneSetRequest { pane: self.pane_ref(), content: content.clone().into(), ..Default::default() });
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
            Last::Known(old) => {
                edits::board_diff(&convert::from_struct(old.board.as_option().unwrap_or(&Struct::default())), board)
            }
            Last::Unset | Last::Unknown => None,
        };
        self.send_diff(&mut last, new, edits);
    }

    fn send_diff(&self, last: &mut Last, new: PaneContent, edits: Option<Vec<PaneEdit>>) {
        match edits {
            Some(edits) if edits.is_empty() => {} // only what's the person's changed (the open tab)
            Some(edits) => self.notify(&PaneEditRequest { pane: self.pane_ref(), edits, ..Default::default() }),
            None => self.notify(&PaneSetRequest {
                pane: self.pane_ref(),
                content: new.clone().into(),
                ..Default::default()
            }),
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
        self.notify(&PaneEditRequest { pane: self.pane_ref(), edits: edits.clone(), ..Default::default() });
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
        self.edit(vec![edits::put_board_items(std::slice::from_ref(item))]);
    }

    pub fn set_title(&self, title: &str) {
        self.notify(&PaneTitleRequest { pane: self.pane_ref(), title: title.to_string(), ..Default::default() });
    }

    /// Close it from the extension's side: what it showed is gone, say.
    pub fn close(&self, reason: &str) {
        if self.drop_it(reason) {
            self.0.handle.notify(&PaneClosedRequest {
                pane: self.pane_ref(),
                reason: reason.to_string(),
                ..Default::default()
            });
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
        lock(&self.0.on_context)
            .push(Arc::new(move |pane: &Pane, context: &PaneContext| f(pane, context).into_result()));
        self
    }

    /// `f(pane, attached)` when the number of the app's paths attached to a `video` pane changes: start capturing
    /// when it's above 0, and stop at 0, so a camera's light is off when no one is watching.
    pub fn on_viewers<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, u64) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        lock(&self.0.on_viewers).push(Arc::new(move |pane: &Pane, attached: u64| f(pane, attached).into_result()));
        self
    }

    /// `f(pane, grant)` when the node gives a person control of what a `video` pane shows (`pane.control`: the
    /// request has `until`, `by` and `device`), and `f(pane, None)` when it ends: stopped, past `until`, or the pane
    /// closed. While a grant holds, the node passes the app's `pointer`, `scroll`, `key` and `text` inputs to
    /// [`Pane::on_input`]. Return an error (a [`Refused`] saying what's missing, such as a permission the machine
    /// hasn't given) to turn a grant down: the node doesn't keep it, and the app shows why. A pane with no handler
    /// refuses every grant.
    pub fn on_control<F, R>(&self, f: F) -> &Self
    where
        F: Fn(&Pane, Option<&PaneControlRequest>) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        *lock(&self.0.control) =
            Some(Arc::new(move |pane: &Pane, grant: Option<&PaneControlRequest>| f(pane, grant).into_result()));
        self
    }

    /// How many of the app's paths are attached to a `video` pane now, as the node last said.
    pub fn viewers(&self) -> u64 {
        self.0.viewers.load(Ordering::SeqCst)
    }

    /// One frame of a `video` pane: a whole JPEG, `width` by `height`, captured at `captured_ms` (Unix time in ms).
    /// Lossy and newest-wins, so it's never held: one sent before the pane's open is answered, after it's closed, or
    /// too big once sealed ([`VIDEO_FRAME_MAX`]: logged once) is dropped, and false says so.
    pub fn frame(&self, jpeg: &[u8], width: u16, height: u16, captured_ms: u64) -> bool {
        if jpeg.len() + SEALING > VIDEO_FRAME_MAX {
            if !self.0.too_big.swap(true, Ordering::SeqCst) {
                let size = jpeg.len();
                self.0
                    .handle
                    .log(format!("pane {}: a frame of {size} bytes is over the 4 MiB a frame: dropped", self.0.id));
            }
            return false;
        }
        let frame = PaneFrameRequest {
            pane: self.pane_ref(),
            captured_ms: captured_ms as i64,
            width: i32::from(width),
            height: i32::from(height),
            jpeg: jpeg.to_vec(),
            ..Default::default()
        };
        self.send_now(&frame)
    }

    /// Sound for a `video` pane that opened with `audio`: one or more whole ADTS frames of AAC, the first sample
    /// captured at `captured_ms` (Unix time in ms), about every 20 to 100 ms. Unlike frames the node queues sound in
    /// order (up to 500 ms a path), so send all of it, in order. Like frames, sound sent before the open is answered
    /// or after the close is dropped, and so is more than [`AUDIO_MAX`] at once (logged once); false says so.
    pub fn audio(&self, aac: &[u8], captured_ms: u64) -> bool {
        if aac.len() > AUDIO_MAX {
            if !self.0.too_much_sound.swap(true, Ordering::SeqCst) {
                let size = aac.len();
                self.0.handle.log(format!("pane {}: {size} bytes of sound at once is over 64 KiB: dropped", self.0.id));
            }
            return false;
        }
        let sound = PaneAudioRequest {
            pane: self.pane_ref(),
            captured_ms: captured_ms as i64,
            sound: Some(Sound::Aac(aac.to_vec())),
            ..Default::default()
        };
        self.send_now(&sound)
    }

    /// Sent at once, or not at all: before the open is answered or after the close, there's nobody to send it to.
    fn send_now<N: Notification>(&self, n: &N) -> bool {
        let gate = lock(&self.0.gate);
        if gate.closed.is_some() || gate.held.is_some() {
            return false;
        }
        self.0.handle.send(&json!({"jsonrpc": "2.0", "method": N::METHOD, "params": to_json(n)}));
        true
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
pub(crate) fn message(
    h: &Handle,
    method: &str,
    params: Value,
    rid: Option<Value>,
    opener: Option<(OpenFn, Option<String>)>,
) {
    if method == "pane.open" {
        let request: PaneOpenRequest = match from_params(params, method) {
            Ok(request) => request,
            Err(e) => return error(h, rid, format!("{e:#}")),
        };
        let pid = request.pane.as_option().map(|p| p.id.clone()).unwrap_or_default();
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
            on_viewers: Mutex::default(),
            control: Mutex::new(None),
            viewers: AtomicU64::new(0),
            too_big: AtomicBool::new(false),
            too_much_sound: AtomicBool::new(false),
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
        "pane.context" => {
            from_params::<PaneContextRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option())
        }
        "pane.viewers" => {
            from_params::<PaneViewersRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option())
        }
        "pane.control" => {
            from_params::<PaneControlRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option())
        }
        _ => from_params::<PaneCloseRequest>(params.clone(), method).ok().and_then(|r| r.pane.into_option()),
    }
    .map(|p| p.id)
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
            let context = from_params::<PaneContextRequest>(params, method)
                .ok()
                .and_then(|r| r.context.into_option())
                .unwrap_or_default();
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Context(context));
            }
        }
        "pane.control" => {
            let Ok(grant) = from_params::<PaneControlRequest>(params, method) else {
                return error(h, rid, "pane.control isn't its message".into());
            };
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Control(grant, rid));
            }
        }
        "pane.viewers" => {
            let attached =
                from_params::<PaneViewersRequest>(params, method).map(|r| r.attached).unwrap_or(0).max(0) as u64;
            pane.0.viewers.store(attached, Ordering::SeqCst);
            if let Some(tx) = lock(&pane.0.queue).as_ref() {
                let _ = tx.send(Message::Viewers(attached));
            }
        }
        _ => {
            let event = from_params::<PaneInputRequest>(params, method)
                .ok()
                .and_then(|r| r.event.into_option())
                .unwrap_or_default();
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
            Message::Viewers(attached) => {
                let handlers = lock(&pane.0.on_viewers).clone();
                for f in handlers {
                    if let Err(e) = guarded(|| f(&pane, attached)) {
                        h.log(format!("pane {} on_viewers: {e:?}", pane.0.id));
                    }
                }
            }
            Message::Input(event, rid) => {
                let f = lock(&pane.0.input).clone();
                let outcome = match f {
                    None => Err(Refused::new("this pane takes no input").into()),
                    Some(f) => guarded(|| f(&pane, &event)),
                };
                answer(
                    &pane,
                    rid,
                    outcome,
                    to_json(PaneInputResponse::default()),
                    &format!("input {} {}", event.kind, event.json()),
                );
            }
            Message::Control(grant, rid) => {
                let f = lock(&pane.0.control).clone();
                // no `until`: nobody controls it now
                let given = grant.until.is_set().then_some(&grant);
                let outcome = match (f, given) {
                    (Some(f), _) => guarded(|| f(&pane, given)),
                    (None, Some(_)) => Err(Refused::new("this pane can't be controlled").into()),
                    (None, None) => Ok(()),
                };
                answer(&pane, rid, outcome, to_json(PaneControlResponse::default()), "control");
            }
        }
    }
}

/// The answer to the node's request `rid`: `result`, or the error's text (logged unless it's a [`Refused`]).
fn answer(pane: &Pane, rid: Option<Value>, outcome: anyhow::Result<()>, result: Value, what: &str) {
    let Some(rid) = rid else { return };
    let h = &pane.0.handle;
    h.send(&match outcome {
        Ok(()) => json!({"jsonrpc": "2.0", "id": rid, "result": result}),
        Err(e) => {
            if !caused_by::<Refused>(&e) {
                h.log(format!("pane {} {what}: {e:?}", pane.0.id));
            }
            json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": text_or(&e, "refused")}})
        }
    });
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
        (PaneFormat::PANE_FORMAT_VIDEO, Some(First::Video(v))) => (is_video(v), VIDEO_WANTS),
        (PaneFormat::PANE_FORMAT_CALL, Some(First::Call(_))) => (true, ""),
        (PaneFormat::PANE_FORMAT_BLOCKS, _) => (false, "its content, as blocks"),
        (PaneFormat::PANE_FORMAT_BOARD, _) => (false, "its content, as a board"),
        (PaneFormat::PANE_FORMAT_PAGE, _) => (false, "its page, with a path"),
        (PaneFormat::PANE_FORMAT_VIDEO, _) => (false, VIDEO_WANTS),
        _ => (false, "its call"),
    };
    if !ok {
        bail!("a {} pane opens with {what}", pane_format_word(format));
    }
    Ok(answer)
}

const VIDEO_WANTS: &str =
    "its video, with its codec, a size of 1 to 65535 each way, a rate above 0 to 240, and any audio's codec, rate 8000-96000 and channels 1 or 2";

/// What a `video` pane opens with: its codec, size and rate, and any sound's codec, rate and channels.
fn is_video(v: &Video) -> bool {
    let sound = v.audio.as_option().is_none_or(|a| (8000..=96000).contains(&a.rate) && (1..=2).contains(&a.channels));
    (1..=65535).contains(&v.width) && (1..=65535).contains(&v.height) && v.fps > 0.0 && v.fps <= 240.0 && sound
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
