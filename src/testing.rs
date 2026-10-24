//! A stand-in node, for an extension's own tests: the twin of Pointman's `mb_extension_testing`.
//!
//! ```no_run
//! use pointman_extension::testing::StandIn;
//! use serde_json::json;
//!
//! # fn main() -> Result<(), pointman_extension::testing::Failed> {
//! let node = StandIn::new(".")                       // the folder with extension.toml
//!     .binary("target/debug/hello")                  // in a test: env!("CARGO_BIN_EXE_<name>")
//!     .settings(json!({"greeting": "Hi"}))
//!     .secrets([("hello.key", "k")])
//!     .start()?;
//! let done = node.run("hello.greet", json!({"name": "James"}))?;
//! assert_eq!(done.result, json!({"text": "Hi, James!"}));
//! assert_eq!(node.events()["hello.last"], json!({"name": "James"}));
//! # Ok(()) }
//! ```
//!
//! It starts the extension the way a node does (its extension.toml's `[run]` command, from its
//! folder, with only the node's environment), says hello with the settings, sends commands and
//! waits for them, and answers what the extension asks: secrets only within its permissions, as the
//! node does, from the fakes it's given. What it doesn't do: the node's restarts, checkpoint files,
//! params checked against the manifest's schemas, or anything reaching the board.
//!
//! Panes open as the node opens them, and keep their content as an app would, with each set and
//! patch applied ([`StandIn::open_pane`], [`PaneView`]). It checks blocks only for an id each,
//! unique in the pane, and a type; the node checks them fully.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::protocol::pane_audio_request::Sound as SoundOf;
use crate::protocol::pane_open_response::First;
use crate::protocol::pane_open_response::Video;
use crate::protocol::{
    CheckpointRequest, CommandCancelRequest, CommandId, CommandStartRequest, DoneRequest, EventRequest, FailedRequest,
    InitializeRequest, InitializeResponse, OutputRequest, PaneClosedRequest, PaneEditRequest, PaneInputRequest,
    PaneOpenRequest, PaneOpenResponse, PaneSetRequest, PaneTitleRequest, PaneViewer, ProgressRequest, SecretGetRequest,
    SecretGetResponse, SecretListRequest, SecretListResponse,
};
use crate::protocol::{PaneAudioRequest, PaneControlRequest, PaneFrameRequest, PaneViewersRequest};
use crate::v1::{Block, MachineId, MemberRef, PaneContent, PaneContext, PaneFormat, PaneId, PaneInput};
use crate::{
    convert, edits, from_params, guarded, id_text, load_manifest, lock, resolve, Request, AUDIO_MAX, PROTOCOL, SEALING,
    VIDEO_FRAME_MAX,
};

/// What a stand-in passes on from its own environment, as a node does: never the board's tokens.
pub const ENV_KEEP: [&str; 7] = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR", "PYTHONUTF8"];
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A pane's frame, as the stream's limit: 1 MiB.
pub const FRAME_MAX: usize = 1 << 20;
/// How many of a video pane's newest frames a [`PaneView`] keeps.
pub const VIDEO_FRAMES_KEPT: usize = 32;
/// How many of a video pane's newest `pane.audio`s a [`PaneView`] keeps.
pub const AUDIO_KEPT: usize = 256;

type MethodFn = Arc<dyn Fn(Value) -> anyhow::Result<Value> + Send + Sync>;
type ThenFn = Box<dyn FnOnce(&Value) + Send>;

/// A command failed, or the extension didn't answer. Its Display is the extension's own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub message: String,
    /// The extension said it's worth trying again later.
    pub retry: bool,
    pub cancelled: bool,
}

impl Failed {
    fn new(message: impl Into<String>) -> Self {
        Failed { message: message.into(), retry: false, cancelled: false }
    }

    fn retry(message: impl Into<String>) -> Self {
        Failed { message: message.into(), retry: true, cancelled: false }
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failed {}

/// A finished command: what it returned, and what it reported on the way, as the messages it sent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Done {
    pub result: Value,
    pub progress: Vec<ProgressRequest>,
    pub outputs: Vec<OutputRequest>,
    pub checkpoints: Vec<Value>,
}

/// How to run one command: `RunOptions { cid: Some("c1".into()), ..Default::default() }`.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub timeout: Duration,
    /// Its id (`cmd-<n>` when not given): what [`StandIn::cancel`] takes.
    pub cid: Option<String>,
    /// The state to resume from, as a node sends after a restart.
    pub checkpoint: Option<Value>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions { timeout: CALL_TIMEOUT, cid: None, checkpoint: None }
    }
}

/// One frame a `video` pane sent (`pane.frame`), its JPEG decoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VideoFrame {
    pub captured_ms: u64,
    pub width: u16,
    pub height: u16,
    pub jpeg: Vec<u8>,
}

/// One `pane.audio` a `video` pane sent, its AAC decoded: whole ADTS frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sound {
    pub captured_ms: u64,
    pub aac: Vec<u8>,
    /// How many ADTS frames it holds.
    pub frames: usize,
}

/// How to open one pane: `PaneOptions { context: PaneContext { item: Some("MB-1".into()), ..Default::default() }, ..Default::default() }`.
#[derive(Debug, Clone)]
pub struct PaneOptions {
    /// What the person is looking at.
    pub context: PaneContext,
    /// Who it's for (`james` when not given).
    pub viewer: PaneViewer,
    pub timeout: Duration,
}

impl Default for PaneOptions {
    fn default() -> Self {
        let person =
            MemberRef { kind: Some(crate::v1::member_ref::Kind::Handle("james".into())), ..Default::default() };
        PaneOptions {
            context: PaneContext::default(),
            viewer: PaneViewer { person: person.into(), ..Default::default() },
            timeout: CALL_TIMEOUT,
        }
    }
}

/// The node's rule: named outright, or under a prefix ending `.*` (`rota.*` covers `rota.work`).
pub fn allows_secret(patterns: &[impl AsRef<str>], name: &str) -> bool {
    patterns.iter().map(AsRef::as_ref).any(|p| {
        p == name
            || p.strip_suffix('*')
                .is_some_and(|prefix| p.ends_with(".*") && name.starts_with(prefix) && name.len() > prefix.len())
    })
}

/// A stand-in node: build it, [`start`](Self::start) it, then run commands. It stops the extension
/// when dropped.
pub struct StandIn {
    root: PathBuf,
    binary: Option<PathBuf>,
    settings: Value,
    secrets: BTreeMap<String, String>,
    methods: HashMap<String, MethodFn>,
    state_dir: Option<PathBuf>,
    roots: Vec<PathBuf>,
    env: Vec<(OsString, OsString)>,
    node: String,
    machine: String,
    live: Option<Live>,
}

struct Live {
    shared: Arc<Shared>,
    tmp: Option<PathBuf>,
    state_dir: PathBuf,
    work_dir: PathBuf,
    info: Value,
    stopped: AtomicBool,
}

impl StandIn {
    /// `root`: the extension's folder, with its extension.toml.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        StandIn {
            root: root.into(),
            binary: None,
            settings: json!({}),
            secrets: BTreeMap::new(),
            methods: HashMap::new(),
            state_dir: None,
            roots: Vec::new(),
            env: Vec::new(),
            node: "stand-in".into(),
            machine: "test-machine".into(),
            live: None,
        }
    }

    /// The program to start instead of the `[run]` command's first word (its arguments stay): the
    /// binary cargo built, `env!("CARGO_BIN_EXE_<name>")`.
    pub fn binary(mut self, path: impl Into<PathBuf>) -> Self {
        self.binary = Some(path.into());
        self
    }

    /// This machine's settings for it, as the node sends them (the extension adds the defaults).
    pub fn settings(mut self, settings: Value) -> Self {
        self.settings = settings;
        self
    }

    /// The vault as the extension may see it, by name; `secret.get` and `secret.list` only reach
    /// the names its `permissions.secrets` cover.
    pub fn secrets<K: Into<String>, V: Into<String>>(mut self, secrets: impl IntoIterator<Item = (K, V)>) -> Self {
        self.secrets.extend(secrets.into_iter().map(|(k, v)| (k.into(), v.into())));
        self
    }

    pub fn secret(self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.secrets([(name.into(), value.into())])
    }

    /// Anything more it may ask the node (an mcp extension's `oauth.load`), answered by `f` with the request and
    /// the answer as their messages. An error goes back to the extension as the node's reason.
    pub fn on<R, F>(mut self, f: F) -> Self
    where
        R: Request,
        F: Fn(R) -> anyhow::Result<R::Response> + Send + Sync + 'static,
    {
        let f: MethodFn = Arc::new(move |params| {
            let request: R = from_params(params, R::METHOD)?;
            Ok(serde_json::to_value(f(request)?)?)
        });
        self.methods.insert(R::METHOD.to_string(), f);
        self
    }

    /// The same by method name, with the params and the answer as JSON: for a fake that needn't be typed.
    pub fn method<F, R>(mut self, name: &str, f: F) -> Self
    where
        F: Fn(Value) -> anyhow::Result<R> + Send + Sync + 'static,
        R: Serialize,
    {
        let f: MethodFn = Arc::new(move |params| Ok(serde_json::to_value(f(params)?)?));
        self.methods.insert(name.to_string(), f);
        self
    }

    /// Where the node keeps its state (`<state_dir>/extensions/<id>`); a temporary folder, removed
    /// when it stops, if not given.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// The folders it may use besides its own work and state folders (`h.allowed(path)`).
    pub fn roots<P: Into<PathBuf>>(mut self, roots: impl IntoIterator<Item = P>) -> Self {
        self.roots.extend(roots.into_iter().map(Into::into));
        self
    }

    /// An environment variable for its process, on top of the node's own few.
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.env.push((key.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    /// The node's name, as `initialize` gives it (`stand-in`).
    pub fn node(mut self, name: impl Into<String>) -> Self {
        self.node = name.into();
        self
    }

    /// The machine's name, as `initialize` gives it (`test-machine`).
    pub fn machine(mut self, name: impl Into<String>) -> Self {
        self.machine = name.into();
        self
    }

    /// Start it and say hello. [`info`](Self::info) has its answer (protocol, id, version).
    pub fn start(mut self) -> Result<Self, Failed> {
        let manifest = load_manifest(&self.root.join("extension.toml")).map_err(|e| Failed::new(format!("{e:#}")))?;
        let id = manifest["extension"]["id"]
            .as_str()
            .ok_or_else(|| Failed::new("extension.toml has no [extension] id"))?
            .to_string();
        if manifest.get("run").is_none() {
            return Err(Failed::new(format!("{id} has no [run]: nothing to start")));
        }
        let mut argv: Vec<OsString> = manifest["run"]["command"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(OsString::from)
            .collect();
        if argv.is_empty() {
            return Err(Failed::new(format!("{id}'s [run] command is empty")));
        }
        // a path like `bin/hello` is the extension folder's, as on a node; a binary given here is
        // the test's (cargo's target folder)
        let (first, base) = match &self.binary {
            Some(binary) => (binary.clone(), std::env::current_dir().unwrap_or_default()),
            None => (PathBuf::from(&argv[0]), self.root.clone()),
        };
        argv[0] = if first.is_relative() && first.components().count() > 1 { base.join(first) } else { first }.into();

        let (tmp, base) = match &self.state_dir {
            Some(dir) => (None, dir.clone()),
            None => {
                let dir = temp_dir(&id).map_err(|e| Failed::new(format!("can't make a temporary folder: {e}")))?;
                (Some(dir.clone()), dir)
            }
        };
        let state_dir = base.join("extensions").join(&id);
        let work_dir = state_dir.join("work");
        std::fs::create_dir_all(&work_dir)
            .map_err(|e| Failed::new(format!("can't make {}: {e}", work_dir.display())))?;

        let mut command = Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(&self.root).env_clear();
        for key in ENV_KEEP {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command.env("PYTHONUNBUFFERED", "1");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        command.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|e| {
            if let Some(tmp) = &tmp {
                let _ = std::fs::remove_dir_all(tmp);
            }
            let hint = if self.binary.is_none() {
                " (for a binary cargo built, use .binary(env!(\"CARGO_BIN_EXE_<name>\")))"
            } else {
                ""
            };
            Failed::new(format!("can't start {} in {}: {e}{hint}", argv[0].to_string_lossy(), self.root.display()))
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        let allowed = manifest["permissions"]["secrets"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(String::from)
            .collect();
        let shared = Arc::new(Shared {
            id,
            allowed,
            secrets: self.secrets.clone(),
            methods: self.methods.clone(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            ids: AtomicU64::new(0),
            pending: Mutex::default(),
            then: Mutex::default(),
            panes: Mutex::default(),
            runs: Mutex::default(),
            events: Mutex::default(),
            changed: Condvar::new(),
            asked: Mutex::default(),
            stderr: Mutex::default(),
            gone: AtomicBool::new(false),
        });
        if let Some(stdout) = stdout {
            let s = shared.clone();
            thread::spawn(move || s.read(stdout));
        }
        if let Some(mut stderr) = stderr {
            let s = shared.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    lock(&s.stderr).push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            });
        }
        self.live = Some(Live {
            shared: shared.clone(),
            tmp,
            state_dir: state_dir.clone(),
            work_dir: work_dir.clone(),
            info: Value::Null,
            stopped: AtomicBool::new(false),
        });

        let roots: Vec<String> = self.roots.iter().map(|r| resolve(r).to_string_lossy().into_owned()).collect();
        let hello = InitializeRequest {
            protocol: PROTOCOL.into(),
            node: self.node.clone(),
            machine: MachineId { value: self.machine.clone(), ..Default::default() }.into(),
            settings: convert::to_struct(&self.settings).into(),
            work_dir: work_dir.to_string_lossy().into_owned(),
            state_dir: state_dir.to_string_lossy().into_owned(),
            roots,
            ..Default::default()
        };
        let info = match shared.request(
            "initialize",
            serde_json::to_value(&hello).unwrap_or_default(),
            Duration::from_secs(20),
        ) {
            Ok(info) => info,
            Err(e) => {
                self.stop();
                return Err(e);
            }
        };
        let said: InitializeResponse = crate::from_json(info.clone()).unwrap_or_default();
        if said.protocol.as_known() != Some(PROTOCOL) {
            self.stop();
            return Err(Failed::new(format!(
                "{} speaks protocol {}, nodes {}",
                shared.id,
                info["protocol"],
                buffa::Enumeration::to_i32(&PROTOCOL)
            )));
        }
        if let Some(live) = self.live.as_mut() {
            live.info = info;
        }
        Ok(self)
    }

    fn live(&self) -> Result<&Live, Failed> {
        self.live.as_ref().ok_or_else(|| Failed::new("the stand-in isn't started: call .start()"))
    }

    /// Its answer to `initialize`: `protocol`, `id`, `version`.
    pub fn info(&self) -> Value {
        self.live.as_ref().map(|l| l.info.clone()).unwrap_or(Value::Null)
    }

    /// The extension's work folder (`<state_dir>/work`); each command gets `<work>/<id>`.
    ///
    /// # Panics
    /// Before [`start`](Self::start).
    pub fn work_dir(&self) -> &Path {
        &self.live.as_ref().expect("the stand-in isn't started").work_dir
    }

    /// The extension's state folder, `<state_dir>/extensions/<id>`.
    ///
    /// # Panics
    /// Before [`start`](Self::start).
    pub fn state_dir_path(&self) -> &Path {
        &self.live.as_ref().expect("the stand-in isn't started").state_dir
    }

    /// Start one command and wait up to 30 s for it.
    pub fn run(&self, kind: &str, params: Value) -> Result<Done, Failed> {
        self.run_with(kind, params, RunOptions::default())
    }

    pub fn run_timeout(&self, kind: &str, params: Value, timeout: Duration) -> Result<Done, Failed> {
        self.run_with(kind, params, RunOptions { timeout, ..Default::default() })
    }

    pub fn run_with(&self, kind: &str, params: Value, options: RunOptions) -> Result<Done, Failed> {
        let live = self.live()?;
        let s = &live.shared;
        let cid = options.cid.unwrap_or_else(|| format!("cmd-{}", s.ids.fetch_add(1, Ordering::SeqCst) + 1));
        let (tx, rx) = mpsc::channel();
        lock(&s.runs).insert(cid.clone(), (Done::default(), tx));
        let params = if params.is_null() { json!({}) } else { params };
        let start = CommandStartRequest {
            id: CommandId { value: cid.clone(), ..Default::default() }.into(),
            kind: kind.to_string(),
            params: convert::to_struct(&params).into(),
            work: live.work_dir.join(&cid).to_string_lossy().into_owned(),
            checkpoint: options.checkpoint.as_ref().map(convert::to_proto).into(),
            ..Default::default()
        };
        let start = serde_json::to_value(start).unwrap_or_default();
        if let Err(e) = s.request("command.start", start, CALL_TIMEOUT) {
            // it stopped before saying it had the command: its run's own end (done, or stopped) says more
            let ended = rx.try_recv().ok();
            lock(&s.runs).remove(&cid);
            return ended.unwrap_or(Err(e));
        }
        let finished = rx.recv_timeout(options.timeout);
        lock(&s.runs).remove(&cid);
        match finished {
            Ok(outcome) => outcome,
            Err(_) => Err(Failed::retry(format!("{kind} didn't finish in {:.0} s", options.timeout.as_secs_f64()))),
        }
    }

    /// Cancel a running command, by its id: whether the extension was running it.
    pub fn cancel(&self, cid: &str) -> Result<bool, Failed> {
        let id = CommandId { value: cid.to_string(), ..Default::default() };
        let request = CommandCancelRequest { id: id.into(), ..Default::default() };
        let answer = self.live()?.shared.request(
            "command.cancel",
            serde_json::to_value(request).unwrap_or_default(),
            CALL_TIMEOUT,
        )?;
        Ok(crate::from_json::<crate::protocol::CommandCancelResponse>(answer).map(|a| a.found).unwrap_or(false))
    }

    /// Ask it one of its own methods (`ext.method`), as the node does, and wait up to 30 s.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Failed> {
        self.call_timeout(method, params, CALL_TIMEOUT)
    }

    pub fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Failed> {
        let params = if params.is_null() { json!({}) } else { params };
        self.live()?.shared.request(method, params, timeout).map(convert::whole_numbers)
    }

    /// Open one of its `[[panes]]`, as the node does for an app, and wait for its answer.
    pub fn open_pane(&self, open: &str, target: Value) -> Result<PaneView, Failed> {
        self.open_pane_with(open, target, PaneOptions::default())
    }

    pub fn open_pane_with(&self, open: &str, target: Value, options: PaneOptions) -> Result<PaneView, Failed> {
        let s = &self.live()?.shared;
        let pid = format!("pane_{}", s.ids.fetch_add(1, Ordering::SeqCst) + 1);
        let view = Arc::new(View { id: pid.clone(), state: Mutex::default(), changed: Condvar::new() });
        lock(&s.panes).insert(pid.clone(), view.clone());
        let params = serde_json::to_value(PaneOpenRequest {
            pane: PaneId { value: pid.clone(), ..Default::default() }.into(),
            open: open.to_string(),
            target: convert::to_struct(&target).into(),
            context: options.context.clone().into(),
            viewer: options.viewer.clone().into(),
            ..Default::default()
        })
        .unwrap_or_default();
        let opened = view.clone();
        let then: ThenFn = Box::new(move |answer: &Value| opened.took("opened", answer, false));
        if let Err(e) = s.request_then("pane.open", params, options.timeout, Some(then)) {
            lock(&s.panes).remove(&pid);
            return Err(e);
        }
        Ok(PaneView { shared: s.clone(), view })
    }

    /// Each event's latest data, by name, as an object: `node.events()["hello.last"]`.
    pub fn events(&self) -> Value {
        self.live.as_ref().map(|l| Value::Object(lock(&l.shared.events).clone())).unwrap_or_else(|| json!({}))
    }

    /// Wait until the extension has sent this event.
    pub fn wait_event(&self, name: &str, timeout: Duration) -> Result<Value, Failed> {
        self.wait_event_where(name, timeout, |_| true)
    }

    /// Wait until this event's latest data passes `test`.
    pub fn wait_event_where(
        &self,
        name: &str,
        timeout: Duration,
        test: impl Fn(&Value) -> bool,
    ) -> Result<Value, Failed> {
        let s = &self.live()?.shared;
        let deadline = Instant::now() + timeout;
        let mut events = lock(&s.events);
        loop {
            if let Some(data) = events.get(name).filter(|data| test(data)) {
                return Ok(data.clone());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || s.gone.load(Ordering::SeqCst) {
                let last = events.get(name).map(|d| format!(" (last: {d})")).unwrap_or_default();
                return Err(Failed::new(format!("no such {name} event in {:.1} s{last}", timeout.as_secs_f64())));
            }
            events = s.changed.wait_timeout(events, left).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    /// What the extension asked the node, in order: (method, params).
    pub fn asked(&self) -> Vec<(String, Value)> {
        self.live.as_ref().map(|l| lock(&l.shared.asked).clone()).unwrap_or_default()
    }

    /// What it wrote to stderr (its log), so far.
    pub fn log(&self) -> String {
        self.live.as_ref().map(|l| lock(&l.shared.stderr).clone()).unwrap_or_default()
    }

    /// Whether its process is still running.
    pub fn running(&self) -> bool {
        self.live.as_ref().is_some_and(|l| l.shared.running())
    }

    /// Say shutdown, close its stdin, and wait up to 5 s for it to exit (then kill it). Once only;
    /// dropping the stand-in does it too.
    pub fn stop(&self) {
        let Some(live) = &self.live else { return };
        if live.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        let s = &live.shared;
        if s.running() {
            let _ = s.request("shutdown", json!({}), Duration::from_secs(5));
        }
        lock(&s.stdin).take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut child = lock(&s.child);
            match child.try_wait() {
                Ok(None) if Instant::now() < deadline => {
                    drop(child);
                    thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                _ => break,
            }
        }
        if let Some(tmp) = &live.tmp {
            let _ = std::fs::remove_dir_all(tmp);
        }
    }
}

impl Drop for StandIn {
    fn drop(&mut self) {
        self.stop();
    }
}

// ------------------------------------------------------------------ talking

type Answer = Result<Value, Failed>;
/// A command on its way: what it has reported so far, and where its end goes.
type Running = (Done, Sender<Result<Done, Failed>>);

struct Shared {
    id: String,
    allowed: Vec<String>,
    secrets: BTreeMap<String, String>,
    methods: HashMap<String, MethodFn>,
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    ids: AtomicU64,
    pending: Mutex<HashMap<u64, Sender<Answer>>>,
    /// What to do with an answer on the reading thread, before anything sent after it.
    then: Mutex<HashMap<u64, ThenFn>>,
    panes: Mutex<HashMap<String, Arc<View>>>,
    runs: Mutex<HashMap<String, Running>>,
    events: Mutex<Map<String, Value>>,
    changed: Condvar,
    asked: Mutex<Vec<(String, Value)>>,
    stderr: Mutex<String>,
    gone: AtomicBool,
}

impl Shared {
    fn running(&self) -> bool {
        !self.gone.load(Ordering::SeqCst) && matches!(lock(&self.child).try_wait(), Ok(None))
    }

    fn log_tail(&self) -> String {
        let log = lock(&self.stderr);
        let start = log.len().saturating_sub(2000);
        let start = (start..log.len()).find(|i| log.is_char_boundary(*i)).unwrap_or(log.len());
        log[start..].to_string()
    }

    fn send(&self, msg: &Value) -> Result<(), Failed> {
        let not_running = || Failed::retry(format!("{} isn't running:\n{}", self.id, self.log_tail()));
        if !self.running() {
            return Err(not_running());
        }
        let mut stdin = lock(&self.stdin);
        let writer = stdin.as_mut().ok_or_else(not_running)?;
        let mut line = msg.to_string();
        line.push('\n');
        writer.write_all(line.as_bytes()).and_then(|()| writer.flush()).map_err(|_| not_running())
    }

    fn request(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Failed> {
        self.request_then(method, params, timeout, None)
    }

    fn request_then(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        then: Option<ThenFn>,
    ) -> Result<Value, Failed> {
        let rid = self.ids.fetch_add(1, Ordering::SeqCst) + 1;
        let (tx, rx) = mpsc::channel();
        lock(&self.pending).insert(rid, tx);
        if let Some(then) = then {
            lock(&self.then).insert(rid, then);
        }
        let answer = self
            .send(&json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}))
            .map(|()| rx.recv_timeout(timeout));
        lock(&self.pending).remove(&rid);
        lock(&self.then).remove(&rid);
        match answer? {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(failed)) => Err(failed),
            Err(_) => Err(Failed::new(format!(
                "{} didn't answer {method} in {:.0} s:\n{}",
                self.id,
                timeout.as_secs_f64(),
                self.log_tail()
            ))),
        }
    }

    fn read(self: Arc<Self>, stdout: impl Read) {
        let mut reader = BufReader::new(stdout);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let msg = match serde_json::from_slice::<Value>(&buf) {
                Ok(msg) if msg.is_object() => msg,
                _ => {
                    let raw = String::from_utf8_lossy(&buf);
                    let clipped: String = raw.chars().take(200).collect();
                    lock(&self.stderr).push_str(&format!("[stdout, not JSON] {clipped:?}\n"));
                    continue;
                }
            };
            if msg.get("method").is_none() {
                let rid = msg.get("id").and_then(Value::as_u64);
                if let Some(tx) = rid.and_then(|rid| lock(&self.pending).get(&rid).cloned()) {
                    let answer = match msg.get("error") {
                        Some(e) if !e.is_null() => Err(Failed::new(
                            e.get("message").and_then(Value::as_str).map(String::from).unwrap_or_else(|| e.to_string()),
                        )),
                        _ => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let then = rid.and_then(|rid| lock(&self.then).remove(&rid));
                    if let (Some(then), Ok(result)) = (then, &answer) {
                        then(result);
                    }
                    let _ = tx.send(answer);
                }
            } else if msg.get("id").is_some() {
                let s = self.clone();
                thread::spawn(move || s.answer(msg));
            } else {
                self.notification(&msg);
            }
        }
        // it's gone: nobody waits forever (its runs first, so a command.start it never answered
        // finds its run's end)
        self.gone.store(true, Ordering::SeqCst);
        let tail = self.log_tail();
        for (_, (_, tx)) in lock(&self.runs).drain() {
            let _ = tx.send(Err(Failed::retry(format!("{} stopped:\n{tail}", self.id))));
        }
        for tx in lock(&self.pending).values() {
            let _ = tx.send(Err(Failed::retry(format!("{} stopped before answering:\n{tail}", self.id))));
        }
        let _events = lock(&self.events);
        self.changed.notify_all();
    }

    /// The extension asking the node something: its fakes, or secrets within its permissions.
    fn answer(&self, msg: Value) {
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = match msg.get("params") {
            None | Some(Value::Null) => json!({}),
            Some(p) => p.clone(),
        };
        lock(&self.asked).push((method.clone(), params.clone()));
        let reply: Result<Value, String> = if let Some(f) = self.methods.get(&method) {
            guarded(|| f(params)).map_err(|e| format!("{e:#}"))
        } else if method == "secret.list" {
            let prefix = crate::from_json::<SecretListRequest>(params).map(|r| r.prefix).unwrap_or_default();
            let names: Vec<String> = self
                .secrets
                .keys()
                .filter(|n| n.starts_with(&prefix) && allows_secret(&self.allowed, n))
                .cloned()
                .collect();
            Ok(serde_json::to_value(SecretListResponse { names, ..Default::default() }).unwrap_or_default())
        } else if method == "secret.get" {
            let name = crate::from_json::<SecretGetRequest>(params).map(|r| r.name).unwrap_or_default();
            if !allows_secret(&self.allowed, &name) {
                Err(format!("{} may not use the secret '{name}': it isn't in its permissions", self.id))
            } else {
                match self.secrets.get(&name) {
                    Some(value) => {
                        Ok(serde_json::to_value(SecretGetResponse { value: value.clone(), ..Default::default() })
                            .unwrap_or_default())
                    }
                    None => Err(format!("no secret '{name}' in the vault")),
                }
            }
        } else {
            Err(format!("no method {method}"))
        };
        let rid = msg.get("id").cloned().unwrap_or(Value::Null);
        let _ = self.send(&match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": rid, "result": result}),
            Err(message) => json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32000, "message": message}}),
        });
    }

    fn notification(&self, msg: &Value) {
        let method = msg["method"].as_str().unwrap_or_default();
        let p = &msg["params"];
        if method.starts_with("pane.") {
            let pane = p.get("pane").and_then(|pane| pane.get("value")).map(id_text).unwrap_or_default();
            let view = lock(&self.panes).get(&pane).cloned();
            if let Some(view) = view {
                view.took(method, p, true);
            }
            return;
        }
        if method == "event" {
            let Ok(event) = crate::from_json::<EventRequest>(p.clone()) else { return };
            if event.name.starts_with(&format!("{}.", self.id)) {
                lock(&self.events).insert(event.name, convert::from_struct(&event.data));
                self.changed.notify_all();
            }
            return;
        }
        let cid = p.get("id").and_then(|id| id.get("value")).map(id_text).unwrap_or_default();
        let mut runs = lock(&self.runs);
        let Some((record, _)) = runs.get_mut(&cid) else { return };
        match method {
            "progress" => record.progress.extend(crate::from_json::<ProgressRequest>(p.clone()).ok()),
            "checkpoint" => {
                if let Ok(c) = crate::from_json::<CheckpointRequest>(p.clone()) {
                    record.checkpoints.push(c.state.as_option().map(convert::from_proto).unwrap_or(Value::Null));
                }
            }
            "output" => record.outputs.extend(crate::from_json::<OutputRequest>(p.clone()).ok()),
            "done" => {
                if let (Some((mut record, tx)), Ok(done)) =
                    (runs.remove(&cid), crate::from_json::<DoneRequest>(p.clone()))
                {
                    record.result = done.result.as_option().map(convert::from_proto).unwrap_or(Value::Null);
                    let _ = tx.send(Ok(record));
                }
            }
            "failed" => {
                if let (Some((_, tx)), Ok(failed)) = (runs.remove(&cid), crate::from_json::<FailedRequest>(p.clone())) {
                    let message = if failed.error.is_empty() { "failed".to_string() } else { failed.error };
                    let _ = tx.send(Err(Failed { message, retry: failed.retry, cancelled: failed.cancelled }));
                }
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------ panes

/// A pane as an app sees it: its content, with every set and edit applied, and its inputs.
pub struct PaneView {
    shared: Arc<Shared>,
    view: Arc<View>,
}

struct View {
    id: String,
    state: Mutex<ViewState>,
    changed: Condvar,
}

#[derive(Default)]
struct ViewState {
    opened: bool,
    format: Option<PaneFormat>,
    title: Option<String>,
    content: PaneContent,
    path: Option<String>,
    closed: Option<String>,
    frames: Vec<(String, Value)>,
    problems: Vec<String>,
    changes: u64,
    video: Option<Video>,
    video_frames: VecDeque<VideoFrame>,
    frame_count: u64,
    sounds: VecDeque<Sound>,
    sound_count: u64,
}

impl PaneView {
    /// Its id, as core would give it (`pane_3`).
    pub fn id(&self) -> &str {
        &self.view.id
    }

    /// What a `video` pane opened with: its size and rate, and any sound's.
    pub fn video(&self) -> Option<Video> {
        lock(&self.view.state).video.clone()
    }

    /// The newest frames a `video` pane sent (at most [`VIDEO_FRAMES_KEPT`]), oldest first.
    pub fn video_frames(&self) -> Vec<VideoFrame> {
        lock(&self.view.state).video_frames.iter().cloned().collect()
    }

    /// How many frames a `video` pane has sent in all.
    pub fn frame_count(&self) -> u64 {
        lock(&self.view.state).frame_count
    }

    /// The newest sound a `video` pane sent (at most [`AUDIO_KEPT`] `pane.audio`s), oldest first.
    pub fn sounds(&self) -> Vec<Sound> {
        lock(&self.view.state).sounds.iter().cloned().collect()
    }

    /// How many `pane.audio`s a `video` pane has sent in all.
    pub fn sound_count(&self) -> u64 {
        lock(&self.view.state).sound_count
    }

    /// Blocks, board, page, video or call.
    pub fn format(&self) -> Option<PaneFormat> {
        lock(&self.view.state).format
    }

    /// Its format as a word: `blocks`, `board`, `page`, `video` or `call`.
    pub fn format_word(&self) -> Option<&'static str> {
        self.format().map(crate::pane::pane_format_word)
    }

    pub fn title(&self) -> Option<String> {
        lock(&self.view.state).title.clone()
    }

    /// Its content now: the blocks, or the board.
    pub fn content(&self) -> PaneContent {
        lock(&self.view.state).content.clone()
    }

    /// A blocks pane's blocks now.
    pub fn blocks(&self) -> Vec<Block> {
        lock(&self.view.state).content.blocks.clone()
    }

    /// A board pane's board now, as JSON (null for another pane).
    pub fn board(&self) -> Value {
        let state = lock(&self.view.state);
        state.content.board.as_option().map(convert::from_struct).unwrap_or(Value::Null)
    }

    /// A page's path.
    pub fn path(&self) -> Option<String> {
        lock(&self.view.state).path.clone()
    }

    /// The block with this name, anywhere in the tree.
    pub fn block(&self, name: &str) -> Option<Block> {
        edits::find(&lock(&self.view.state).content.blocks, name).cloned()
    }

    /// The board's item with this id, as JSON.
    pub fn item(&self, id: &str) -> Option<Value> {
        self.board()["items"].as_array()?.iter().find(|item| item["id"] == id).cloned()
    }

    /// Why it closed, once it has.
    pub fn closed(&self) -> Option<String> {
        lock(&self.view.state).closed.clone()
    }

    /// What the extension sent after opening, in order: (method, params).
    pub fn frames(&self) -> Vec<(String, Value)> {
        lock(&self.view.state).frames.clone()
    }

    /// What the node would have refused, and why: edits that don't apply (all or none), a set over 1 MiB, a
    /// block with no name or a family, or a name used twice.
    pub fn problems(&self) -> Vec<String> {
        lock(&self.view.state).problems.clone()
    }

    /// Send an input, as the app does, and wait for the extension's answer: its edits are applied by then. A
    /// refusal is a [`Failed`] whose message is the extension's reason.
    pub fn input(&self, event: PaneInput) -> Result<(), Failed> {
        let request = PaneInputRequest {
            pane: PaneId { value: self.view.id.clone(), ..Default::default() }.into(),
            event: event.into(),
            ..Default::default()
        };
        self.shared.request("pane.input", serde_json::to_value(request).unwrap_or_default(), CALL_TIMEOUT).map(drop)
    }

    /// The same with the kind and fields as JSON: `input_json(json!({"kind": "tap", "block": "plus"}))`, `{"kind":
    /// "move", "item": …, "to": …}`.
    pub fn input_json(&self, event: Value) -> Result<(), Failed> {
        let mut fields = event.as_object().cloned().unwrap_or_default();
        let kind = fields.remove("kind").and_then(|k| k.as_str().map(String::from)).unwrap_or_default();
        self.input(PaneInput::new(&kind, Value::Object(fields)))
    }

    /// How many of the app's paths are attached to a `video` pane now, as the node says when one attaches or drops:
    /// `pane.viewers`.
    pub fn viewers(&self, attached: u64) -> Result<(), Failed> {
        let request = PaneViewersRequest {
            pane: PaneId { value: self.view.id.clone(), ..Default::default() }.into(),
            attached: attached as i32,
            ..Default::default()
        };
        self.shared.send(&json!({"jsonrpc": "2.0", "method": "pane.viewers", "params": request}))
    }

    /// The app's hint for a `video` pane, as the node passes it on (`pane.input` of kind `hint`): at most this many
    /// frames a second (0 pauses it) and this wide. Waits for the answer.
    pub fn hint(&self, max_fps: u64, max_width: u64) -> Result<(), Failed> {
        self.input(PaneInput::new("hint", json!({"max_fps": max_fps, "max_width": max_width})))
    }

    /// The app's speaker button for a `video` pane with sound, as the node passes it on: a hint with only `audio`.
    /// Waits for the answer.
    pub fn audio_hint(&self, on: bool) -> Result<(), Failed> {
        self.input(PaneInput::new("hint", json!({"audio": on})))
    }

    /// The node gives a person control of a `video` pane (`pane.control`: `until` in unix seconds, `by` their handle,
    /// and their `device`), or ends it (`until` None). Waits for the extension's answer: a refusal is a [`Failed`]
    /// whose message is its reason.
    pub fn control(&self, until: Option<u64>, by: &str, device: &str) -> Result<(), Failed> {
        let person =
            MemberRef { kind: Some(crate::v1::member_ref::Kind::Handle(by.to_string())), ..Default::default() };
        let request = PaneControlRequest {
            pane: PaneId { value: self.view.id.clone(), ..Default::default() }.into(),
            until: until
                .map(|s| buffa_types::google::protobuf::Timestamp { seconds: s as i64, ..Default::default() })
                .into(),
            by: person.into(),
            device: Some(device.to_string()),
            ..Default::default()
        };
        self.shared.request("pane.control", serde_json::to_value(request).unwrap_or_default(), CALL_TIMEOUT).map(drop)
    }

    /// An input that controls a `video` pane (`{"kind": "pointer", "x": 0.5, "y": 0.5, "phase": "down", "button":
    /// "left", "clicks": 1}`, `scroll`, `key` or `text`), as the node passes it on while a grant holds. Waits for the
    /// answer.
    pub fn control_input(&self, event: Value) -> Result<(), Failed> {
        self.input_json(event)
    }

    /// What the person is looking at changed: `pane.context`.
    pub fn look_at(&self, context: PaneContext) -> Result<(), Failed> {
        let request = crate::protocol::PaneContextRequest {
            pane: PaneId { value: self.view.id.clone(), ..Default::default() }.into(),
            context: context.into(),
            ..Default::default()
        };
        self.shared.send(&json!({"jsonrpc": "2.0", "method": "pane.context", "params": request}))
    }

    /// Close it from the node's side, as core does when the app goes.
    pub fn close(&self, reason: &str) -> Result<(), Failed> {
        let request = crate::protocol::PaneCloseRequest {
            pane: PaneId { value: self.view.id.clone(), ..Default::default() }.into(),
            reason: reason.to_string(),
            ..Default::default()
        };
        self.shared.send(&json!({"jsonrpc": "2.0", "method": "pane.close", "params": request}))?;
        self.view.took("closed", &json!({"reason": reason}), false);
        Ok(())
    }

    /// Wait until `test(view)` holds: for what a periodic task changes.
    pub fn wait(&self, timeout: Duration, test: impl Fn(&PaneView) -> bool) -> Result<(), Failed> {
        let deadline = Instant::now() + timeout;
        loop {
            let seen = lock(&self.view.state).changes;
            if test(self) {
                return Ok(());
            }
            let state = lock(&self.view.state);
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || self.shared.gone.load(Ordering::SeqCst) {
                let mut content = serde_json::to_string(&state.content).unwrap_or_default();
                content.truncate(content.char_indices().nth(600).map_or(content.len(), |(i, _)| i));
                return Err(Failed::new(format!(
                    "pane {} didn't get there in {:.1} s: {content}",
                    self.view.id,
                    timeout.as_secs_f64()
                )));
            }
            if state.changes == seen {
                drop(self.view.changed.wait_timeout(state, left).unwrap_or_else(PoisonError::into_inner));
            }
        }
    }
}

impl View {
    fn took(&self, method: &str, p: &Value, record: bool) {
        let mut state = lock(&self.state);
        if method == "pane.audio" || method == "pane.frame" {
            if !state.opened {
                state.problems.push(format!("{method} before the pane was open"));
            } else if state.closed.is_none() {
                if method == "pane.frame" {
                    match video_frame(state.format, p) {
                        Ok(frame) => {
                            state.frame_count += 1;
                            if state.video_frames.len() == VIDEO_FRAMES_KEPT {
                                state.video_frames.pop_front();
                            }
                            state.video_frames.push_back(frame);
                        }
                        Err(problem) => state.problems.push(problem),
                    }
                } else {
                    match sound(state.format, state.video.as_ref().is_some_and(|v| v.audio.is_set()), p) {
                        Ok(sound) => {
                            state.sound_count += 1;
                            if state.sounds.len() == AUDIO_KEPT {
                                state.sounds.pop_front();
                            }
                            state.sounds.push_back(sound);
                        }
                        Err(problem) => state.problems.push(problem),
                    }
                }
            }
            state.changes += 1;
            self.changed.notify_all();
            return;
        }
        if record {
            state.frames.push((method.to_string(), p.clone()));
        }
        if !state.opened && method != "opened" {
            state.problems.push(format!("{method} before the pane was open"));
        } else if state.closed.is_some() && method != "opened" {
            // in flight as it closed: the node drops it
        } else {
            match method {
                "opened" => match crate::from_json::<PaneOpenResponse>(p.clone()) {
                    Ok(answer) => {
                        state.opened = true;
                        state.format = answer.format.as_known();
                        state.title = answer.title.clone();
                        match answer.first {
                            Some(First::Content(content)) => {
                                let problems = set_problems(state.format, &content);
                                state.problems.extend(problems);
                                state.content = *content;
                            }
                            Some(First::Page(page)) => state.path = Some(page.path),
                            Some(First::Video(video)) => state.video = Some(*video),
                            _ => {}
                        }
                    }
                    Err(e) => state.problems.push(format!("the answer to pane.open isn't its message: {e}")),
                },
                "pane.set" | "pane.edit" if state.format == Some(PaneFormat::PANE_FORMAT_VIDEO) => {
                    state.problems.push(format!("{method} on a video pane, which has frames, not content"))
                }
                "pane.set" => match crate::from_json::<PaneSetRequest>(p.clone()) {
                    Ok(set) => {
                        let content = set.content.into_option().unwrap_or_default();
                        let problems = set_problems(state.format, &content);
                        if problems.is_empty() {
                            state.content = content;
                        }
                        state.problems.extend(problems);
                    }
                    Err(e) => state.problems.push(format!("pane.set isn't its message: {e}")),
                },
                "pane.edit" => match crate::from_json::<PaneEditRequest>(p.clone()) {
                    Ok(request) => match edits::apply(&state.content, &request.edits) {
                        Ok(content) => {
                            let problems = block_problems(&content.blocks);
                            state.problems.extend(problems);
                            state.content = content;
                        }
                        Err(e) => state.problems.push(format!("edits: {e}")),
                    },
                    Err(e) => state.problems.push(format!("pane.edit isn't its message: {e}")),
                },
                "pane.title" => {
                    state.title = crate::from_json::<PaneTitleRequest>(p.clone()).ok().map(|t| t.title);
                }
                "pane.closed" | "closed" => {
                    let reason = match method {
                        "closed" => p["reason"].as_str().map(String::from),
                        _ => crate::from_json::<PaneClosedRequest>(p.clone()).ok().map(|c| c.reason),
                    };
                    state.closed = Some(reason.filter(|r| !r.is_empty()).unwrap_or_else(|| "closed".into()))
                }
                _ => {}
            }
        }
        state.changes += 1;
        self.changed.notify_all();
    }
}

/// A `pane.frame` as the node takes it, or why it would drop it.
fn video_frame(format: Option<PaneFormat>, p: &Value) -> Result<VideoFrame, String> {
    if format != Some(PaneFormat::PANE_FORMAT_VIDEO) {
        let word = format.map_or("formatless", crate::pane::pane_format_word);
        return Err(format!("pane.frame on a {word} pane: only a video pane has frames"));
    }
    let frame: PaneFrameRequest =
        crate::from_json(p.clone()).map_err(|e| format!("pane.frame isn't its message: {e}"))?;
    let jpeg = frame.jpeg;
    if !(jpeg.starts_with(&[0xff, 0xd8]) && jpeg.ends_with(&[0xff, 0xd9])) {
        return Err(format!("pane.frame: {} bytes that aren't a whole JPEG (FF D8 … FF D9)", jpeg.len()));
    }
    if jpeg.len() + SEALING > VIDEO_FRAME_MAX {
        return Err(format!("pane.frame: {} bytes is over the 4 MiB a frame, sealed", jpeg.len()));
    }
    let size = |n: i32| u16::try_from(n).ok().filter(|n| *n > 0);
    let (Some(width), Some(height)) = (size(frame.width), size(frame.height)) else {
        return Err(format!("pane.frame: its size {}x{} isn't 1 to 65535 each way", frame.width, frame.height));
    };
    if p.get("capturedMs").is_none() {
        return Err("pane.frame has no captured_ms".into());
    }
    Ok(VideoFrame { captured_ms: frame.captured_ms as u64, width, height, jpeg })
}

/// A `pane.audio` as the node takes it, or why it would drop it.
fn sound(format: Option<PaneFormat>, has_audio: bool, p: &Value) -> Result<Sound, String> {
    if format != Some(PaneFormat::PANE_FORMAT_VIDEO) {
        let word = format.map_or("formatless", crate::pane::pane_format_word);
        return Err(format!("pane.audio on a {word} pane: only a video pane has sound"));
    }
    if !has_audio {
        return Err("pane.audio on a video pane that opened without audio".into());
    }
    let audio: PaneAudioRequest =
        crate::from_json(p.clone()).map_err(|e| format!("pane.audio isn't its message: {e}"))?;
    let Some(SoundOf::Aac(aac)) = audio.sound else { return Err("pane.audio: a video pane's sound is aac".into()) };
    if aac.is_empty() || aac.len() > AUDIO_MAX {
        return Err(format!("pane.audio: {} bytes, not 1 to 64 KiB", aac.len()));
    }
    let frames =
        adts_frames(&aac).ok_or_else(|| format!("pane.audio: {} bytes that aren't whole ADTS frames", aac.len()))?;
    if p.get("capturedMs").is_none() {
        return Err("pane.audio has no captured_ms".into());
    }
    Ok(Sound { captured_ms: audio.captured_ms as u64, aac, frames })
}

/// How many ADTS frames `aac` is, if it's nothing but whole ones: each a sync word (twelve 1 bits), a header of 7
/// bytes (9 with a CRC), and the length its header says.
pub fn adts_frames(aac: &[u8]) -> Option<usize> {
    let (mut at, mut count) = (0, 0);
    while at < aac.len() {
        let h = aac.get(at..at + 7)?;
        if h[0] != 0xff || h[1] & 0xf0 != 0xf0 {
            return None;
        }
        let header = if h[1] & 1 == 1 { 7 } else { 9 };
        let len = (usize::from(h[3] & 3) << 11) | (usize::from(h[4]) << 3) | usize::from(h[5] >> 5);
        if len < header || at + len > aac.len() {
            return None;
        }
        at += len;
        count += 1;
    }
    (count > 0).then_some(count)
}

fn set_problems(format: Option<PaneFormat>, content: &PaneContent) -> Vec<String> {
    let mut problems = Vec::new();
    let size = serde_json::to_vec(content).map(|b| b.len()).unwrap_or(0);
    if size > FRAME_MAX {
        problems.push(format!("a set of {size} bytes is over the 1 MiB a frame"));
    }
    if format == Some(PaneFormat::PANE_FORMAT_BOARD) {
        problems.extend(board_problems(&content.board.as_option().map(convert::from_struct).unwrap_or(Value::Null)));
    } else {
        problems.extend(block_problems(&content.blocks));
    }
    problems
}

/// projects.md's seven categories: a board's states and items each have one.
pub const CATEGORIES: [&str; 7] = ["triage", "backlog", "todo", "active", "review", "done", "dropped"];

/// The little a stand-in checks of a board: items with an id each, used once, and every state's and
/// item's category one of the seven.
pub fn board_problems(board: &Value) -> Vec<String> {
    let Some(items) = board["items"].as_array() else { return vec!["a board is an object with items".into()] };
    let mut problems = Vec::new();
    let category = |v: &Value| v.as_str().is_some_and(|c| CATEGORIES.contains(&c));
    for (i, state) in board["states"].as_array().into_iter().flatten().enumerate() {
        if !category(&state["category"]) {
            problems.push(format!(
                "states[{i}] ({}): {} isn't one of {}",
                state["name"],
                state["category"],
                CATEGORIES.join(", ")
            ));
        }
    }
    let mut seen: Vec<&str> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        match item["id"].as_str().filter(|id| !id.is_empty()) {
            None => problems.push(format!("items[{i}] has no id")),
            Some(id) if seen.contains(&id) => problems.push(format!("items[{i}]: the id '{id}' is used twice")),
            Some(id) => seen.push(id),
        }
        if item.get("category").is_some() && !category(&item["category"]) {
            problems.push(format!(
                "items[{i}] ({}): {} isn't one of {}",
                item["id"],
                item["category"],
                CATEGORIES.join(", ")
            ));
        }
    }
    problems
}

/// The little a stand-in checks: each block has a name, unique in the pane, and a family.
pub fn block_problems(blocks: &[Block]) -> Vec<String> {
    fn walk(items: &[Block], at: &str, seen: &mut Vec<String>, problems: &mut Vec<String>) {
        for (i, b) in items.iter().enumerate() {
            let here = format!("{at}[{i}]");
            if b.family.is_none() {
                problems.push(format!("{here} has no family (text, chart, list…)"));
            }
            match b.name.as_deref().filter(|n| !n.is_empty()) {
                None => problems.push(format!("{here} has no name")),
                Some(name) if seen.iter().any(|s| s == name) => {
                    problems.push(format!("{here}: the name '{name}' is used twice"))
                }
                Some(name) => seen.push(name.to_string()),
            }
            for (k, kids) in edits::children(b).into_iter().enumerate() {
                walk(kids, &format!("{here}.children{k}"), seen, problems);
            }
        }
    }
    let (mut seen, mut problems) = (Vec::new(), Vec::new());
    walk(blocks, "blocks", &mut seen, &mut problems);
    problems
}

fn temp_dir(id: &str) -> std::io::Result<PathBuf> {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let n = COUNT.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("{id}-{}-{n}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::{allows_secret, block_problems, board_problems};
    use crate::v1::{block::Family, layout, Block, Layout};
    use serde_json::json;

    fn text(name: Option<&str>) -> Block {
        Block { name: name.map(String::from), family: Some(Family::Text(Box::default())), ..Default::default() }
    }

    #[test]
    fn boards_need_the_seven_categories_and_ids_once() {
        let board = json!({"states": [{"name": "In Progress", "category": "started"}, {"name": "Done", "category": "done"}],
                           "items": [{"id": "a", "category": "active"}, {"id": "a"}, {"category": "nope"}]});
        assert_eq!(board_problems(&board), [
            "states[0] (\"In Progress\"): \"started\" isn't one of triage, backlog, todo, active, review, done, dropped",
            "items[1]: the id 'a' is used twice",
            "items[2] has no id",
            "items[2] (null): \"nope\" isn't one of triage, backlog, todo, active, review, done, dropped",
        ]);
        assert_eq!(board_problems(&json!({"items": []})), Vec::<String>::new());
    }

    #[test]
    fn blocks_need_a_name_once_and_a_family() {
        let stack = Block {
            name: Some("s".into()),
            family: Some(Family::Layout(Box::new(Layout {
                kind: Some(layout::Kind::Stack(Box::new(layout::Stack {
                    children: vec![text(Some("a")), text(None), Block { name: Some("n".into()), ..Default::default() }],
                    ..Default::default()
                }))),
                ..Default::default()
            }))),
            ..Default::default()
        };
        assert_eq!(
            block_problems(&[text(Some("a")), stack]),
            [
                "blocks[1].children0[0]: the name 'a' is used twice",
                "blocks[1].children0[1] has no name",
                "blocks[1].children0[2] has no family (text, chart, list…)",
            ]
        );
    }

    #[test]
    fn secrets_by_name_or_under_a_prefix() {
        let allowed = ["rota.*", "shared.key"];
        assert!(allows_secret(&allowed, "rota.work"));
        assert!(allows_secret(&allowed, "rota.work.2"));
        assert!(allows_secret(&allowed, "shared.key"));
        assert!(!allows_secret(&allowed, "rota."));
        assert!(!allows_secret(&allowed, "rota"));
        assert!(!allows_secret(&allowed, "rotary.x"));
        assert!(!allows_secret(&allowed, "shared.key2"));
        assert!(!allows_secret(&[] as &[&str], "rota.work"));
    }
}
