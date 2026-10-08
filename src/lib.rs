//! The extension side of the Pointman node's extension protocol (Pointman's docs/dev/extensions.md,
//! "Running an extension"), for extensions written in Rust: newline-delimited JSON-RPC 2.0 on stdin
//! and stdout, with the log on stderr. The twin of the Python SDK (`mb_extension.py`), message for
//! message.
//!
//! ```no_run
//! use pointman_extension::{Extension, Handle, Job, Retry};
//! use serde_json::json;
//! use std::time::Duration;
//!
//! fn main() -> anyhow::Result<()> {
//!     let mut ext = Extension::new()?;            // reads extension.toml
//!     ext.command("demo.render", |job: &Job| {
//!         job.progress(0.5, "rendering");
//!         let out = job.work.join("out.mp4");
//!         if job.params["app"] == "closed" {
//!             return Err(Retry::new("the app isn't open").into());
//!         }
//!         job.output(&out, "video", "main", json!({}));
//!         Ok(json!({"frames": 250}))
//!     });
//!     ext.every(Duration::from_secs(10), |h: &Handle| h.event("demo.status", json!({"ok": true})), "demo");
//!     ext.run()
//! }
//! ```
//!
//! Commands in the same lane run one at a time, on the lane's own thread, with its periodic tasks
//! between them (an app's API that must only ever be driven from one thread). A command with no
//! lane gets a thread of its own, and so does each of the node's requests to a [`Extension::method`].
//!
//! Panes ([`Extension::pane`], [`Pane`]): the extension answers `pane.open` for each of its
//! manifest's `[[panes]]` with the first content, then keeps it current; each pane's open, inputs,
//! context and close run in order on a thread of its own.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, BufRead, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context as _};
use serde::Serialize;
use serde_json::{json, Map, Value};

mod pane;
pub mod testing;

pub use pane::{board_changes, changes, pointer, Pane, Refused};

/// The protocol version this SDK speaks.
pub const PROTOCOL: u64 = 1;
/// Where extension.toml is (the file or its folder), when it isn't in the working folder or in
/// the folder above the binary's.
pub const MANIFEST_ENV: &str = "POINTMAN_EXTENSION_MANIFEST";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Fail this command, but say it's worth trying again later (the app isn't open, say):
/// `return Err(Retry::new("Resolve isn't open").into())` sends `failed` with `retry: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Retry(String);

impl Retry {
    pub fn new(why: impl Into<String>) -> Self {
        Retry(why.into())
    }
}

impl fmt::Display for Retry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Retry {}

/// The command was cancelled: what [`Job::check`] gives back, and sent as `failed` with
/// `cancelled: true`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// What an `on_start`, `on_settings` or `every` callback may return: nothing, or a `Result`. An
/// error is logged, and `on_settings`'s is the node's answer.
pub trait Outcome {
    fn into_result(self) -> anyhow::Result<()>;
}

impl Outcome for () {
    fn into_result(self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl<E: Into<anyhow::Error>> Outcome for Result<(), E> {
    fn into_result(self) -> anyhow::Result<()> {
        self.map_err(Into::into)
    }
}

type CommandFn = Arc<dyn Fn(&Job) -> anyhow::Result<Value> + Send + Sync>;
type MethodFn = Arc<dyn Fn(Value) -> anyhow::Result<Value> + Send + Sync>;
type HookFn = Arc<dyn Fn(&Handle) -> anyhow::Result<()> + Send + Sync>;

// ------------------------------------------------------------------ talking to the node

struct Shared {
    id: String,
    version: String,
    manifest: Value,
    settings: RwLock<Value>,
    node: RwLock<Value>,
    events: Mutex<HashMap<String, Value>>,
    answers: Mutex<HashMap<String, Sender<Result<Value, String>>>>,
    calls: AtomicU64,
    panes: Mutex<HashMap<String, Pane>>,
}

/// The extension's line to the node: events, requests, secrets, the log, the settings. Cheap to
/// clone, and usable from any thread.
#[derive(Clone)]
pub struct Handle(Arc<Shared>);

impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle").field("id", &self.0.id).finish_non_exhaustive()
    }
}

impl Handle {
    /// The extension's id, from extension.toml.
    pub fn id(&self) -> &str {
        &self.0.id
    }

    /// extension.toml, as JSON.
    pub fn manifest(&self) -> &Value {
        &self.0.manifest
    }

    /// This machine's settings for it, with the defaults from the manifest's `[settings]` schema.
    pub fn settings(&self) -> Value {
        read(&self.0.settings).clone()
    }

    /// What the node said about itself in `initialize`: `protocol`, `node`, `machine`, `work_dir`,
    /// `state_dir` and `roots`.
    pub fn node(&self) -> Value {
        read(&self.0.node).clone()
    }

    /// The panes open now, of one of the manifest's `[[panes]]` (every one, for `""`), once their open
    /// is answered: for a task that keeps them current.
    pub fn panes(&self, open: &str) -> Vec<Pane> {
        lock(&self.0.panes).values().filter(|p| (open.is_empty() || p.open() == open) && p.is_open()).cloned().collect()
    }

    /// A line in the extension's log (stderr, which the node keeps). Never print to stdout: it's
    /// the node's.
    pub fn log(&self, message: impl fmt::Display) {
        eprintln!("{message}");
    }

    /// Send a notification to the node.
    pub fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Report status. Sent only when it changed since the last time.
    ///
    /// # Panics
    /// If `name` doesn't start with the extension's id and a dot.
    pub fn event(&self, name: &str, data: impl Serialize) {
        let prefix = format!("{}.", self.0.id);
        if !name.starts_with(&prefix) {
            panic!("event {name} must start with {prefix}");
        }
        let data = to_json(data);
        let mut events = lock(&self.0.events);
        if events.get(name) == Some(&data) {
            return;
        }
        events.insert(name.to_string(), data.clone());
        self.notify("event", json!({"name": name, "data": data}));
    }

    /// Ask the node something (`secret.list`, `oauth.load`), and wait up to 30 s for its answer.
    /// An error carries the node's reason.
    pub fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.request_timeout(method, params, REQUEST_TIMEOUT)
    }

    pub fn request_timeout(&self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let rid = format!("x{}", self.0.calls.fetch_add(1, Ordering::SeqCst) + 1);
        let (tx, rx) = mpsc::channel();
        lock(&self.0.answers).insert(rid.clone(), tx);
        self.send(&json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}));
        let answer = rx.recv_timeout(timeout);
        lock(&self.0.answers).remove(&rid);
        match answer {
            Err(_) => bail!("the node didn't answer {method}"),
            Ok(Err(reason)) => Err(anyhow!(reason)),
            Ok(Ok(Value::Null)) => Ok(json!({})),
            Ok(Ok(result)) => Ok(result),
        }
    }

    /// A vault secret this extension may use (one `permissions.secrets` names or covers), opened by
    /// the node when it's needed. Never kept in settings or on disk.
    pub fn secret(&self, name: &str) -> anyhow::Result<String> {
        let answer = self.request("secret.get", json!({"name": name}))?;
        match answer.get("value") {
            Some(Value::String(value)) => Ok(value.clone()),
            _ => bail!("the node gave no value for the secret {name}"),
        }
    }

    /// A path a command was given, resolved, if it's inside a folder the node allows (its roots,
    /// or this extension's work and state folders); otherwise a permission error.
    pub fn allowed(&self, path: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
        let given = path.as_ref();
        let resolved = resolve(given);
        let node = self.node();
        let mut roots: Vec<&str> = node
            .get("roots")
            .and_then(Value::as_array)
            .map(|roots| roots.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        roots.extend(["work_dir", "state_dir"].iter().filter_map(|k| node.get(*k).and_then(Value::as_str)));
        if roots.iter().filter(|r| !r.is_empty()).any(|r| resolved.starts_with(resolve(Path::new(r)))) {
            return Ok(resolved);
        }
        let why = format!("{} is outside the folders this node lets extensions use", given.display());
        Err(io::Error::new(io::ErrorKind::PermissionDenied, why).into())
    }

    fn send(&self, msg: &Value) {
        let mut line = msg.to_string();
        line.push('\n');
        let mut out = io::stdout().lock();
        let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    }

    fn reply(&self, rid: Value, result: anyhow::Result<Value>) {
        self.send(&match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": rid, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": format!("{e:#}")}}),
        });
    }

    /// The node answering one of our requests.
    fn answered(&self, rid: Option<&Value>, msg: &Value) {
        let key = rid.map(id_text).unwrap_or_default();
        if let Some(tx) = lock(&self.0.answers).get(&key) {
            let answer = match msg.get("error") {
                Some(e) if !e.is_null() => {
                    Err(e.get("message").and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.to_string()))
                }
                _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
            };
            let _ = tx.send(answer);
        }
    }
}

// ------------------------------------------------------------------ commands

/// One command the node asked for.
pub struct Job {
    pub id: String,
    pub kind: String,
    pub params: Value,
    /// Its own folder, for what it makes.
    pub work: PathBuf,
    /// The last state saved with [`Job::save`], when resuming after a restart.
    pub checkpoint: Option<Value>,
    cancel: Arc<AtomicBool>,
    handle: Handle,
}

impl fmt::Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("params", &self.params)
            .field("work", &self.work)
            .field("checkpoint", &self.checkpoint)
            .field("cancelled", &self.cancelled())
            .finish()
    }
}

impl Job {
    /// Whether the node has cancelled it.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Stop here if the command was cancelled: `job.check()?`.
    pub fn check(&self) -> anyhow::Result<()> {
        if self.cancelled() {
            return Err(Cancelled.into());
        }
        Ok(())
    }

    /// How far it's got: `job.progress(0.5, "rendering")`, or `job.progress(None, None)`.
    pub fn progress<'a>(&self, fraction: impl Into<Option<f64>>, stage: impl Into<Option<&'a str>>) {
        self.progress_detail(fraction, stage, json!({}));
    }

    /// Progress with more detail (frames, ETA, GPU).
    pub fn progress_detail<'a>(
        &self,
        fraction: impl Into<Option<f64>>,
        stage: impl Into<Option<&'a str>>,
        detail: Value,
    ) {
        let (fraction, stage): (Option<f64>, Option<&str>) = (fraction.into(), stage.into());
        let detail = if detail.is_null() { json!({}) } else { detail };
        self.handle.notify("progress", json!({"id": self.id, "fraction": fraction, "stage": stage, "detail": detail}));
    }

    /// A checkpoint: given back as `job.checkpoint` if the command restarts.
    pub fn save(&self, state: impl Serialize) {
        self.handle.notify("checkpoint", json!({"id": self.id, "state": to_json(state)}));
    }

    /// A file it made, for the node to publish: `kind` is an output kind (`video`, `image`…),
    /// `role` is `main`, `proxy`, `poster`, `thumb` or `contact_sheet`.
    pub fn output(&self, path: impl AsRef<Path>, kind: &str, role: &str, meta: Value) {
        let role = if role.is_empty() { "main" } else { role };
        let meta = if meta.is_null() { json!({}) } else { meta };
        let path = path.as_ref().to_string_lossy();
        self.handle.notify("output", json!({"id": self.id, "path": path, "kind": kind, "role": role, "meta": meta}));
    }

    /// The extension's [`Handle`], for events and secrets inside a command.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

// ------------------------------------------------------------------ the extension

/// An extension's process: declare its commands, methods and periodic tasks, then [`run`](Self::run).
pub struct Extension {
    handle: Handle,
    lanes: HashMap<String, Option<String>>,
    commands: HashMap<String, CommandFn>,
    pane_formats: HashMap<String, Option<String>>,
    openers: HashMap<String, pane::OpenFn>,
    methods: HashMap<String, MethodFn>,
    on_start: Vec<HookFn>,
    on_settings: Vec<HookFn>,
    periodic: HashMap<String, Vec<(Duration, HookFn)>>,
    jobs: Mutex<HashMap<String, Arc<AtomicBool>>>,
    queues: Mutex<HashMap<String, Sender<Job>>>,
    stop: AtomicBool,
}

impl Extension {
    /// Reads extension.toml: from `$POINTMAN_EXTENSION_MANIFEST`, else the working folder (a node
    /// starts an extension from its own folder), else the folder above the binary's (`bin/..`).
    pub fn new() -> anyhow::Result<Self> {
        let path = find_manifest(
            std::env::var_os(MANIFEST_ENV).map(PathBuf::from),
            std::env::current_dir().ok(),
            std::env::current_exe().ok(),
        )?;
        Self::from_manifest(path)
    }

    /// Reads this extension.toml (the file, or the folder it's in).
    pub fn from_manifest(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut path = path.as_ref().to_path_buf();
        if path.is_dir() {
            path.push("extension.toml");
        }
        let manifest = load_manifest(&path)?;
        let text = |key: &str| manifest["extension"][key].as_str().map(String::from);
        let id = text("id").with_context(|| format!("{} has no [extension] id", path.display()))?;
        let version = text("version").with_context(|| format!("{} has no [extension] version", path.display()))?;
        let lanes = manifest["commands"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| Some((c["kind"].as_str()?.to_string(), c["lane"].as_str().map(String::from))))
            .collect();
        let pane_formats = manifest["panes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["id"].as_str()?.to_string(), p["format"].as_str().map(String::from))))
            .collect();
        let handle = Handle(Arc::new(Shared {
            id,
            version,
            manifest,
            settings: RwLock::new(json!({})),
            node: RwLock::new(json!({})),
            events: Mutex::default(),
            answers: Mutex::default(),
            calls: AtomicU64::new(0),
            panes: Mutex::default(),
        }));
        Ok(Extension {
            handle,
            lanes,
            commands: HashMap::new(),
            pane_formats,
            openers: HashMap::new(),
            methods: HashMap::new(),
            on_start: Vec::new(),
            on_settings: Vec::new(),
            periodic: HashMap::new(),
            jobs: Mutex::default(),
            queues: Mutex::default(),
            stop: AtomicBool::new(false),
        })
    }

    /// The extension's id.
    pub fn id(&self) -> &str {
        self.handle.id()
    }

    /// extension.toml, as JSON.
    pub fn manifest(&self) -> &Value {
        self.handle.manifest()
    }

    /// A [`Handle`] to the node, for use anywhere (in commands, other threads).
    pub fn handle(&self) -> Handle {
        self.handle.clone()
    }

    /// Run `f` for the command `kind`. What it returns is the command's result (`done`); an error
    /// fails it (`failed`), worth retrying if it's a [`Retry`].
    ///
    /// # Panics
    /// If `kind` isn't one of extension.toml's `[[commands]]`.
    pub fn command<F, R>(&mut self, kind: &str, f: F) -> &mut Self
    where
        F: Fn(&Job) -> anyhow::Result<R> + Send + Sync + 'static,
        R: Serialize,
    {
        assert!(self.lanes.contains_key(kind), "{kind} isn't a command in extension.toml");
        let f: CommandFn = Arc::new(move |job: &Job| Ok(serde_json::to_value(f(job)?)?));
        self.commands.insert(kind.to_string(), f);
        self
    }

    /// Answer one of the node's requests beyond commands (an `mcp` extension's `mcp.call_tool`,
    /// `session.prepare`). Each runs on a thread of its own, so a slow one never holds up the rest.
    pub fn method<F, R>(&mut self, name: &str, f: F) -> &mut Self
    where
        F: Fn(Value) -> anyhow::Result<R> + Send + Sync + 'static,
        R: Serialize,
    {
        let f: MethodFn = Arc::new(move |params| Ok(serde_json::to_value(f(params)?)?));
        self.methods.insert(name.to_string(), f);
        self
    }

    /// Open one of the manifest's `[[panes]]`: `f(pane)` returns its first content, `{"title": …,
    /// "blocks": […]}` (or `"board": {…}`, or a page's `"path"`), and the format comes from the
    /// manifest. An error is the node's answer, and the pane doesn't open.
    ///
    /// # Panics
    /// If `id` isn't one of extension.toml's `[[panes]]`.
    pub fn pane<F, R>(&mut self, id: &str, f: F) -> &mut Self
    where
        F: Fn(&Pane) -> anyhow::Result<R> + Send + Sync + 'static,
        R: Serialize,
    {
        assert!(self.pane_formats.contains_key(id), "{id} isn't a pane in extension.toml");
        let f: pane::OpenFn = Arc::new(move |pane: &Pane| Ok(serde_json::to_value(f(pane)?)?));
        self.openers.insert(id.to_string(), f);
        self
    }

    /// The format each of the manifest's `[[panes]]` has, by id.
    pub fn pane_formats(&self) -> &HashMap<String, Option<String>> {
        &self.pane_formats
    }

    /// Called once, after the node has said hello and given the settings.
    pub fn on_start<F, R>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&Handle) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        self.on_start.push(hook(f));
        self
    }

    /// Called when this machine's settings change (`h.settings()` has the new ones).
    pub fn on_settings<F, R>(&mut self, f: F) -> &mut Self
    where
        F: Fn(&Handle) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        self.on_settings.push(hook(f));
        self
    }

    /// Run `f` every so often on a lane's thread, between its commands.
    pub fn every<F, R>(&mut self, interval: Duration, f: F, lane: &str) -> &mut Self
    where
        F: Fn(&Handle) -> R + Send + Sync + 'static,
        R: Outcome,
    {
        self.periodic.entry(lane.to_string()).or_default().push((interval, hook(f)));
        self
    }

    /// Answer the node until it says `shutdown` or closes stdin.
    pub fn run(self) -> anyhow::Result<()> {
        let ext = Arc::new(self);
        let mut input = io::stdin().lock();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if input.read_until(b'\n', &mut buf).context("reading from the node")? == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&buf);
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(line) {
                Ok(msg) if msg.is_object() => {
                    if !ext.dispatch(msg) {
                        break;
                    }
                }
                _ => ext.handle.log(format!("not JSON: {}", clip(line, 200))),
            }
        }
        ext.stop.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// One message from the node; false once it's said shutdown.
    fn dispatch(self: &Arc<Self>, msg: Value) -> bool {
        let rid = msg.get("id").filter(|id| !id.is_null()).cloned();
        let method = match msg.get("method") {
            None | Some(Value::Null) => {
                self.handle.answered(rid.as_ref(), &msg);
                return true;
            }
            Some(Value::String(m)) => m.clone(),
            Some(other) => other.to_string(),
        };
        let params = match msg.get("params") {
            None | Some(Value::Null) => json!({}),
            Some(p) => p.clone(),
        };
        if let Some(f) = self.methods.get(&method).cloned() {
            let h = self.handle.clone();
            spawn(format!("method-{method}"), move || {
                let result = guarded(|| f(params));
                if let Some(rid) = rid {
                    h.reply(rid, result);
                }
            });
            return true;
        }
        if matches!(method.as_str(), "pane.open" | "pane.input" | "pane.context" | "pane.close") {
            let opener = match method.as_str() {
                "pane.open" => params["open"].as_str().and_then(|open| {
                    Some((self.openers.get(open)?.clone(), self.pane_formats.get(open).cloned().flatten()))
                }),
                _ => None,
            };
            pane::message(&self.handle, &method, &params, rid, opener);
            return true;
        }
        let result = match method.as_str() {
            "initialize" => self.initialize(&params),
            "command.start" => self.start(&params),
            "command.cancel" => self.cancel(&params),
            "settings.changed" => self.settings_changed(&params),
            "shutdown" => Ok(json!({})),
            _ => {
                if let Some(rid) = rid {
                    let error = json!({"code": -32601, "message": format!("no method {method}")});
                    self.handle.send(&json!({"jsonrpc": "2.0", "id": rid, "error": error}));
                }
                return true;
            }
        };
        if let Some(rid) = rid {
            self.handle.reply(rid, result);
        }
        method != "shutdown"
    }

    fn initialize(self: &Arc<Self>, p: &Value) -> anyhow::Result<Value> {
        let node: Map<String, Value> = ["protocol", "node", "machine", "work_dir", "state_dir", "roots"]
            .iter()
            .map(|k| (k.to_string(), p.get(*k).cloned().unwrap_or(Value::Null)))
            .collect();
        *write(&self.handle.0.node) = Value::Object(node);
        *write(&self.handle.0.settings) = self.with_defaults(p.get("settings"));
        for lane in self.periodic.keys() {
            self.lane(lane);
        }
        let ext = self.clone();
        spawn("on-start".into(), move || {
            for f in &ext.on_start {
                if let Err(e) = guarded(|| f(&ext.handle)) {
                    ext.handle.log(format!("on_start: {e:?}"));
                }
            }
        });
        Ok(json!({"protocol": PROTOCOL, "id": self.handle.0.id, "version": self.handle.0.version}))
    }

    fn with_defaults(&self, given: Option<&Value>) -> Value {
        let mut out = Map::new();
        if let Some(props) = self.handle.0.manifest["settings"]["properties"].as_object() {
            for (name, schema) in props {
                if let Some(default) = schema.get("default") {
                    out.insert(name.clone(), default.clone());
                }
            }
        }
        if let Some(Value::Object(given)) = given {
            out.extend(given.clone());
        }
        Value::Object(out)
    }

    fn start(self: &Arc<Self>, p: &Value) -> anyhow::Result<Value> {
        let kind = p.get("kind").and_then(Value::as_str).context("command.start has no kind")?;
        if !self.commands.contains_key(kind) {
            bail!("{kind} isn't a command of {}", self.handle.0.id);
        }
        let cid = p.get("id").filter(|id| !id.is_null()).map(id_text).context("command.start has no id")?;
        let mut jobs = lock(&self.jobs);
        if jobs.contains_key(&cid) {
            return Ok(json!({"accepted": true, "already": true}));
        }
        let node_work = self.handle.node()["work_dir"].as_str().map(String::from);
        let work = p
            .get("work")
            .and_then(Value::as_str)
            .map(String::from)
            .into_iter()
            .chain(node_work)
            .find(|w| !w.is_empty())
            .unwrap_or_else(|| ".".into());
        let work = PathBuf::from(work);
        std::fs::create_dir_all(&work).with_context(|| format!("can't make {}", work.display()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        jobs.insert(cid.clone(), cancel.clone());
        drop(jobs);
        let job = Job {
            id: cid.clone(),
            kind: kind.to_string(),
            params: match p.get("params") {
                None | Some(Value::Null) => json!({}),
                Some(params) => params.clone(),
            },
            work,
            checkpoint: p.get("checkpoint").filter(|c| !c.is_null()).cloned(),
            cancel,
            handle: self.handle.clone(),
        };
        match self.lanes.get(kind).and_then(Option::as_deref).filter(|lane| !lane.is_empty()) {
            Some(lane) => {
                let _ = self.lane(lane).send(job);
            }
            None => {
                let ext = self.clone();
                spawn(format!("cmd-{cid}"), move || ext.execute(job));
            }
        }
        Ok(json!({"accepted": true}))
    }

    fn cancel(&self, p: &Value) -> anyhow::Result<Value> {
        let cid = p.get("id").map(id_text).context("command.cancel has no id")?;
        let found = lock(&self.jobs).get(&cid).map(|c| c.store(true, Ordering::SeqCst)).is_some();
        Ok(json!({"found": found}))
    }

    fn settings_changed(&self, p: &Value) -> anyhow::Result<Value> {
        *write(&self.handle.0.settings) = self.with_defaults(p.get("settings"));
        for f in &self.on_settings {
            guarded(|| f(&self.handle))?;
        }
        Ok(json!({}))
    }

    /// The lane's queue, and its thread, started the first time it's needed.
    fn lane(self: &Arc<Self>, lane: &str) -> Sender<Job> {
        let mut queues = lock(&self.queues);
        if let Some(tx) = queues.get(lane) {
            return tx.clone();
        }
        let (tx, rx) = mpsc::channel();
        queues.insert(lane.to_string(), tx.clone());
        let ext = self.clone();
        let name = lane.to_string();
        spawn(format!("lane-{lane}"), move || ext.lane_loop(&name, rx));
        tx
    }

    fn lane_loop(&self, lane: &str, rx: mpsc::Receiver<Job>) {
        let start = Instant::now();
        let mut tasks: Vec<(Duration, HookFn, Instant)> =
            self.periodic.get(lane).into_iter().flatten().map(|(every, f)| (*every, f.clone(), start)).collect();
        while !self.stop.load(Ordering::SeqCst) {
            let now = Instant::now();
            for (every, f, due) in tasks.iter_mut() {
                if now >= *due {
                    *due = now + *every;
                    if let Err(e) = guarded(|| f(&self.handle)) {
                        self.handle.log(format!("every ({lane}): {e:?}"));
                    }
                }
            }
            let due = tasks
                .iter()
                .map(|(_, _, due)| due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_secs(1));
            match rx.recv_timeout(due.clamp(Duration::from_millis(50), Duration::from_secs(1))) {
                Ok(job) => self.execute(job),
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    fn execute(&self, job: Job) {
        let outcome = if job.cancelled() {
            Err(Cancelled.into())
        } else {
            let f = self.commands[&job.kind].clone();
            guarded(|| f(&job))
        };
        let (h, id) = (&self.handle, &job.id);
        match outcome {
            Ok(result) => h.notify("done", json!({"id": id, "result": result})),
            Err(e) if caused_by::<Cancelled>(&e) => {
                h.notify("failed", json!({"id": id, "error": "cancelled", "retry": false, "cancelled": true}))
            }
            Err(e) if caused_by::<Retry>(&e) => {
                h.notify("failed", json!({"id": id, "error": text_or(&e, "try again later"), "retry": true}))
            }
            Err(e) => {
                h.log(format!("{} {id} failed: {e:?}", job.kind));
                h.notify("failed", json!({"id": id, "error": text_or(&e, "failed"), "retry": false}));
            }
        }
        lock(&self.jobs).remove(id);
    }
}

// ------------------------------------------------------------------ the manifest

fn find_manifest(env: Option<PathBuf>, cwd: Option<PathBuf>, exe: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = env.filter(|p| !p.as_os_str().is_empty()) {
        return Ok(path);
    }
    let mut tried = Vec::new();
    let mut candidates: Vec<PathBuf> = cwd.into_iter().map(|cwd| cwd.join("extension.toml")).collect();
    if let Some(exe) = exe {
        let real = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());
        for exe in [exe, real] {
            if let Some(root) = exe.parent().and_then(Path::parent) {
                candidates.push(root.join("extension.toml"));
            }
        }
    }
    for path in candidates {
        if path.is_file() {
            return Ok(path);
        }
        tried.push(path.display().to_string());
    }
    bail!("no extension.toml (looked for {}); set {MANIFEST_ENV} to it", tried.join(", "))
}

pub(crate) fn load_manifest(path: &Path) -> anyhow::Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("can't read {}", path.display()))?;
    let table: toml::Table = toml::from_str(&text).with_context(|| format!("{} isn't valid TOML", path.display()))?;
    Ok(toml_to_json(toml::Value::Table(table)))
}

fn toml_to_json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(s) => Value::String(s),
        toml::Value::Integer(i) => Value::from(i),
        toml::Value::Float(f) => serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number),
        toml::Value::Boolean(b) => Value::Bool(b),
        toml::Value::Datetime(d) => Value::String(d.to_string()),
        toml::Value::Array(items) => Value::Array(items.into_iter().map(toml_to_json).collect()),
        toml::Value::Table(table) => Value::Object(table.into_iter().map(|(k, v)| (k, toml_to_json(v))).collect()),
    }
}

// ------------------------------------------------------------------ small things

fn hook<F, R>(f: F) -> HookFn
where
    F: Fn(&Handle) -> R + Send + Sync + 'static,
    R: Outcome,
{
    Arc::new(move |h: &Handle| f(h).into_result())
}

/// Runs `f`, turning a panic into an error, so one bad command or task never takes a lane down.
pub(crate) fn guarded<T>(f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
    match panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            Err(anyhow!("panicked: {message}"))
        }
    }
}

fn caused_by<T: std::error::Error + 'static>(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| cause.is::<T>())
}

fn text_or(e: &anyhow::Error, otherwise: &str) -> String {
    let text = format!("{e:#}");
    if text.is_empty() {
        otherwise.to_string()
    } else {
        text
    }
}

/// An id as text: `"7"` for 7, as the Python SDK's `str(id)`.
pub(crate) fn id_text(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

pub(crate) fn to_json(value: impl Serialize) -> Value {
    serde_json::to_value(value).unwrap_or_else(|e| Value::String(format!("<not JSON: {e}>")))
}

fn clip(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

fn spawn(name: String, f: impl FnOnce() + Send + 'static) {
    if let Err(e) = thread::Builder::new().name(name.clone()).spawn(f) {
        eprintln!("can't start thread {name}: {e}");
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

/// A path made absolute, `~` expanded and symlinks resolved, whether or not it exists (as Python's
/// `Path.expanduser().resolve()`).
pub(crate) fn resolve(path: &Path) -> PathBuf {
    let mut path = path.to_path_buf();
    if let Ok(rest) = path.strip_prefix("~") {
        if let Some(home) = std::env::var_os("HOME") {
            path = PathBuf::from(home).join(rest);
        }
    }
    if path.is_relative() {
        path = std::env::current_dir().unwrap_or_default().join(path);
    }
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Prefix(_) | Component::RootDir => out.push(part.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if out.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                    if let Ok(real) = std::fs::canonicalize(&out) {
                        out = real;
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pointman-extension-unit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const MANIFEST: &str = r#"
[extension]
id = "unit"
version = "1.2.3"

[settings]
type = "object"
[settings.properties.greeting]
type = "string"
default = "Hello"
[settings.properties.count]
type = "integer"
default = 2
[settings.properties.free]
type = "string"

[[commands]]
kind = "unit.a"
lane = "main"

[[commands]]
kind = "unit.b"
"#;

    #[test]
    fn finds_the_manifest_from_the_env_then_the_cwd_then_beside_the_binary() {
        let root = temp("find");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("extension.toml"), MANIFEST).unwrap();
        let elsewhere = temp("find-elsewhere");
        let exe = root.join("bin").join("unit");
        let manifest = root.join("extension.toml");

        let env = Some(PathBuf::from("/given/extension.toml"));
        assert_eq!(find_manifest(env, Some(root.clone()), None).unwrap(), PathBuf::from("/given/extension.toml"));
        assert_eq!(find_manifest(None, Some(root.clone()), None).unwrap(), manifest);
        assert_eq!(find_manifest(None, Some(elsewhere.clone()), Some(exe)).unwrap(), manifest);
        let e = find_manifest(None, Some(elsewhere.clone()), None).unwrap_err().to_string();
        assert!(e.contains("no extension.toml") && e.contains(MANIFEST_ENV), "{e}");
    }

    #[test]
    fn reads_the_manifest_and_its_lanes() {
        let root = temp("read");
        std::fs::write(root.join("extension.toml"), MANIFEST).unwrap();
        let ext = Extension::from_manifest(&root).unwrap();
        assert_eq!(ext.id(), "unit");
        assert_eq!(ext.manifest()["settings"]["properties"]["count"]["default"], 2);
        assert_eq!(ext.lanes["unit.a"].as_deref(), Some("main"));
        assert_eq!(ext.lanes["unit.b"], None);
    }

    #[test]
    #[should_panic(expected = "unit.nope isn't a command in extension.toml")]
    fn a_command_must_be_in_the_manifest() {
        let root = temp("unknown");
        std::fs::write(root.join("extension.toml"), MANIFEST).unwrap();
        let mut ext = Extension::from_manifest(&root).unwrap();
        ext.command("unit.nope", |_job: &Job| Ok(()));
    }

    #[test]
    fn settings_get_the_schemas_defaults() {
        let root = temp("defaults");
        std::fs::write(root.join("extension.toml"), MANIFEST).unwrap();
        let ext = Extension::from_manifest(&root).unwrap();
        assert_eq!(ext.with_defaults(None), json!({"greeting": "Hello", "count": 2}));
        assert_eq!(
            ext.with_defaults(Some(&json!({"count": 5, "other": true}))),
            json!({"greeting": "Hello", "count": 5, "other": true})
        );
    }

    #[test]
    fn toml_becomes_json() {
        let table: toml::Table =
            toml::from_str("a = 1\nb = 1.5\nc = [true, \"x\"]\nd = 1979-05-27\n[e]\nf = {g = 2}").unwrap();
        assert_eq!(
            toml_to_json(toml::Value::Table(table)),
            json!({"a": 1, "b": 1.5, "c": [true, "x"], "d": "1979-05-27", "e": {"f": {"g": 2}}})
        );
    }

    #[test]
    fn resolve_follows_symlinks_and_dots_like_python() {
        let root = resolve(&temp("resolve"));
        std::fs::create_dir_all(root.join("real/inner")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        assert_eq!(resolve(&root.join("real/./inner/../inner")), root.join("real/inner"));
        assert_eq!(resolve(&root.join("real/not/yet/../there")), root.join("real/not/there"));
        #[cfg(unix)]
        assert_eq!(resolve(&root.join("link/inner")), root.join("real/inner"));
    }

    #[test]
    fn errors_say_why_through_context() {
        let e = anyhow::Error::from(Retry::new("inner")).context("outer");
        assert!(caused_by::<Retry>(&e) && !caused_by::<Cancelled>(&e));
        assert_eq!(text_or(&e, "x"), "outer: inner");
        assert_eq!(text_or(&anyhow::Error::from(Retry::new("")), "try again later"), "try again later");
        let panicked = guarded::<()>(|| panic!("boom")).unwrap_err();
        assert_eq!(panicked.to_string(), "panicked: boom");
    }

    /// Only that these compile: progress with or without a fraction and a stage.
    #[allow(dead_code)]
    fn progress_takes_none_too(job: &Job) {
        job.progress(None, None);
        job.progress(0.1, None);
        job.progress(None, "waiting");
    }
}
