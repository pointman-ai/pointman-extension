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

use std::collections::{BTreeMap, HashMap};
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

use crate::{guarded, id_text, load_manifest, lock, resolve, PROTOCOL};

/// What a stand-in passes on from its own environment, as a node does: never the board's tokens.
pub const ENV_KEEP: [&str; 7] = ["PATH", "HOME", "USER", "LANG", "LC_ALL", "TMPDIR", "PYTHONUTF8"];
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A pane's frame, as the stream's limit: 1 MiB.
pub const FRAME_MAX: usize = 1 << 20;

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

/// A finished command: what it returned, and what it reported on the way.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Done {
    pub result: Value,
    pub progress: Vec<Progress>,
    pub outputs: Vec<Output>,
    pub checkpoints: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    pub fraction: Option<f64>,
    pub stage: Option<String>,
    pub detail: Value,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    pub path: PathBuf,
    pub kind: String,
    pub role: String,
    pub meta: Value,
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

/// How to open one pane: `PaneOptions { context: json!({"thread": "thr_1"}), ..Default::default() }`.
#[derive(Debug, Clone)]
pub struct PaneOptions {
    /// What the person is looking at.
    pub context: Value,
    /// Who: `{person, device}` (`james` on `test-device` when not given).
    pub viewer: Value,
    pub timeout: Duration,
}

impl Default for PaneOptions {
    fn default() -> Self {
        PaneOptions {
            context: json!({}),
            viewer: json!({"person": "james", "device": "test-device"}),
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

    /// Anything more it may ask the node (an mcp extension's `oauth.load`), answered by `f`. An
    /// error goes back to the extension as the node's reason.
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
        let hello = json!({
            "protocol": PROTOCOL, "node": self.node, "machine": self.machine, "settings": self.settings,
            "work_dir": work_dir, "state_dir": state_dir, "roots": roots,
        });
        let info = match shared.request("initialize", hello, Duration::from_secs(20)) {
            Ok(info) => info,
            Err(e) => {
                self.stop();
                return Err(e);
            }
        };
        if info["protocol"] != json!(PROTOCOL) {
            self.stop();
            return Err(Failed::new(format!("{} speaks protocol {}, nodes {PROTOCOL}", shared.id, info["protocol"])));
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
        let start = json!({
            "id": cid, "kind": kind, "params": params,
            "work": live.work_dir.join(&cid), "checkpoint": options.checkpoint,
        });
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

    /// Cancel a running command, by its id: `{"found": bool}`.
    pub fn cancel(&self, cid: &str) -> Result<Value, Failed> {
        self.live()?.shared.request("command.cancel", json!({"id": cid}), CALL_TIMEOUT)
    }

    /// Ask it one of its own methods (`ext.method`), as the node does, and wait up to 30 s.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, Failed> {
        self.call_timeout(method, params, CALL_TIMEOUT)
    }

    pub fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Failed> {
        let params = if params.is_null() { json!({}) } else { params };
        self.live()?.shared.request(method, params, timeout)
    }

    /// New settings for this machine, as `settings.changed`.
    pub fn change_settings(&self, settings: Value) -> Result<(), Failed> {
        self.live()?.shared.request("settings.changed", json!({"settings": settings}), CALL_TIMEOUT).map(drop)
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
        let target = if target.is_null() { json!({}) } else { target };
        let params = json!({
            "pane": pid, "open": open, "target": target, "context": options.context, "viewer": options.viewer,
        });
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
            let prefix = params["prefix"].as_str().unwrap_or_default();
            let names: Vec<&String> =
                self.secrets.keys().filter(|n| n.starts_with(prefix) && allows_secret(&self.allowed, n)).collect();
            Ok(json!({"names": names}))
        } else if method == "secret.get" {
            let name = params["name"].as_str().unwrap_or_default();
            if !allows_secret(&self.allowed, name) {
                Err(format!("{} may not use the secret '{name}': it isn't in its permissions", self.id))
            } else {
                match self.secrets.get(name) {
                    Some(value) => Ok(json!({"value": value})),
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
            let view = lock(&self.panes).get(&p.get("pane").map(id_text).unwrap_or_default()).cloned();
            if let Some(view) = view {
                view.took(method, p, true);
            }
            return;
        }
        if method == "event" {
            let name = p["name"].as_str().unwrap_or_default();
            if name.starts_with(&format!("{}.", self.id)) {
                let data = match p.get("data") {
                    None | Some(Value::Null) => json!({}),
                    Some(data) => data.clone(),
                };
                lock(&self.events).insert(name.to_string(), data);
                self.changed.notify_all();
            }
            return;
        }
        let cid = p.get("id").map(id_text).unwrap_or_default();
        let mut runs = lock(&self.runs);
        let Some((record, _)) = runs.get_mut(&cid) else { return };
        let text = |key: &str| p[key].as_str().map(String::from);
        match method {
            "progress" => record.progress.push(Progress {
                fraction: p["fraction"].as_f64(),
                stage: text("stage"),
                detail: p["detail"].clone(),
            }),
            "checkpoint" => record.checkpoints.push(p["state"].clone()),
            "output" => record.outputs.push(Output {
                path: PathBuf::from(text("path").unwrap_or_default()),
                kind: text("kind").unwrap_or_default(),
                role: text("role").unwrap_or_default(),
                meta: p["meta"].clone(),
            }),
            "done" => {
                if let Some((mut record, tx)) = runs.remove(&cid) {
                    record.result = p["result"].clone();
                    let _ = tx.send(Ok(record));
                }
            }
            "failed" => {
                if let Some((_, tx)) = runs.remove(&cid) {
                    let message = text("error").filter(|e| !e.is_empty()).unwrap_or_else(|| "failed".into());
                    let flag = |key: &str| p[key].as_bool().unwrap_or(false);
                    let _ = tx.send(Err(Failed { message, retry: flag("retry"), cancelled: flag("cancelled") }));
                }
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------ panes

/// A pane as an app sees it: its content, with every set and patch applied, and its inputs.
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
    format: Option<String>,
    title: Option<String>,
    content: Value,
    path: Option<String>,
    closed: Option<String>,
    frames: Vec<(String, Value)>,
    problems: Vec<String>,
    changes: u64,
}

impl PaneView {
    /// Its id, as core would give it (`pane_3`).
    pub fn id(&self) -> &str {
        &self.view.id
    }

    /// `blocks`, `board` or `page`.
    pub fn format(&self) -> Option<String> {
        lock(&self.view.state).format.clone()
    }

    pub fn title(&self) -> Option<String> {
        lock(&self.view.state).title.clone()
    }

    /// Its content now: the blocks (an array) or the board (an object); null for a page.
    pub fn content(&self) -> Value {
        lock(&self.view.state).content.clone()
    }

    /// A page's path.
    pub fn path(&self) -> Option<String> {
        lock(&self.view.state).path.clone()
    }

    /// The block with this id, anywhere in the tree.
    pub fn block(&self, id: &str) -> Option<Value> {
        let state = lock(&self.view.state);
        let at = state.content.as_array().and_then(|blocks| find_block(blocks, id))?;
        get(&state.content, &at).cloned()
    }

    /// The board's item with this id.
    pub fn item(&self, id: &str) -> Option<Value> {
        let state = lock(&self.view.state);
        state.content["items"].as_array()?.iter().find(|item| item["id"] == id).cloned()
    }

    /// Why it closed, once it has.
    pub fn closed(&self) -> Option<String> {
        lock(&self.view.state).closed.clone()
    }

    /// What the extension sent after opening, in order: (method, params).
    pub fn frames(&self) -> Vec<(String, Value)> {
        lock(&self.view.state).frames.clone()
    }

    /// What the node would have refused, and why: a patch that doesn't apply, a set over 1 MiB, a
    /// block with no id or type, or an id used twice.
    pub fn problems(&self) -> Vec<String> {
        lock(&self.view.state).problems.clone()
    }

    /// Send an input, as the app does (`json!({"kind": "tap", "block": "plus"})`, `{"kind": "move",
    /// "item": …, "to": …}`), and wait for the extension's answer: its patch is applied by then. A
    /// refusal is a [`Failed`] whose message is the extension's reason.
    pub fn input(&self, event: Value) -> Result<(), Failed> {
        self.shared.request("pane.input", json!({"pane": self.view.id, "event": event}), CALL_TIMEOUT).map(drop)
    }

    /// What the person is looking at changed: `pane.context`.
    pub fn look_at(&self, context: Value) -> Result<(), Failed> {
        self.shared.send(&json!({"jsonrpc": "2.0", "method": "pane.context",
                                 "params": {"pane": self.view.id, "context": context}}))
    }

    /// Close it from the node's side, as core does when the app goes.
    pub fn close(&self, reason: &str) -> Result<(), Failed> {
        let params = json!({"pane": self.view.id, "reason": reason});
        self.shared.send(&json!({"jsonrpc": "2.0", "method": "pane.close", "params": params}))?;
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
                return Err(Failed::new(format!(
                    "pane {} didn't get there in {:.1} s: {}",
                    self.view.id,
                    timeout.as_secs_f64(),
                    state.content
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
        if record {
            state.frames.push((method.to_string(), p.clone()));
        }
        if !state.opened && method != "opened" {
            state.problems.push(format!("{method} before the pane was open"));
        } else if state.closed.is_some() && method != "opened" {
            // in flight as it closed: the node drops it
        } else {
            match method {
                "opened" => {
                    state.opened = true;
                    state.format = p["format"].as_str().map(String::from);
                    state.title = p["title"].as_str().map(String::from);
                    state.path = p["path"].as_str().map(String::from);
                    let content = content_of(&state, p);
                    let problems = set_problems(state.format.as_deref(), &content);
                    state.problems.extend(problems);
                    state.content = content;
                }
                "pane.set" => {
                    let content = content_of(&state, p);
                    let problems = set_problems(state.format.as_deref(), &content);
                    if problems.is_empty() {
                        state.content = content;
                    }
                    state.problems.extend(problems);
                }
                "pane.patch" => match apply_patch(&state.content, &p["patch"]) {
                    Ok(content) => {
                        let problems = match state.format.as_deref() {
                            Some("blocks") => block_problems(&content),
                            Some("board") => board_problems(&content),
                            _ => Vec::new(),
                        };
                        state.problems.extend(problems);
                        state.content = content;
                    }
                    Err(e) => state.problems.push(format!("patch: {e}")),
                },
                "pane.title" => state.title = p["title"].as_str().map(String::from),
                "pane.closed" | "closed" => {
                    state.closed = Some(p["reason"].as_str().filter(|r| !r.is_empty()).unwrap_or("closed").to_string())
                }
                _ => {}
            }
        }
        state.changes += 1;
        self.changed.notify_all();
    }
}

fn content_of(state: &ViewState, p: &Value) -> Value {
    match state.format.as_deref() {
        Some("blocks") => p["blocks"].clone(),
        Some("board") => p["board"].clone(),
        _ => Value::Null,
    }
}

fn set_problems(format: Option<&str>, content: &Value) -> Vec<String> {
    let mut problems = Vec::new();
    if format == Some("page") {
        return problems;
    }
    let size = serde_json::to_vec(content).map(|b| b.len()).unwrap_or(0);
    if size > FRAME_MAX {
        problems.push(format!("a set of {size} bytes is over the 1 MiB a frame"));
    }
    if format == Some("blocks") {
        problems.extend(block_problems(content));
    } else {
        problems.extend(board_problems(content));
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

/// The little a stand-in checks: each block has a type and an id, unique in the pane.
pub fn block_problems(blocks: &Value) -> Vec<String> {
    fn walk(items: &[Value], at: &str, seen: &mut Vec<String>, problems: &mut Vec<String>) {
        for (i, b) in items.iter().enumerate() {
            let here = format!("{at}[{i}]");
            let Some(kind) = b["type"].as_str().filter(|t| !t.is_empty()) else {
                problems.push(format!("{here} has no type"));
                continue;
            };
            match b["id"].as_str().filter(|id| !id.is_empty()) {
                None => problems.push(format!("{here} ({kind}) has no id")),
                Some(id) if seen.iter().any(|s| s == id) => {
                    problems.push(format!("{here}: the id '{id}' is used twice"))
                }
                Some(id) => seen.push(id.to_string()),
            }
            if let Some(children) = b["children"].as_array() {
                walk(children, &format!("{here}.children"), seen, problems);
            }
            for (t, tab) in b["tabs"].as_array().into_iter().flatten().enumerate() {
                if let Some(children) = tab["children"].as_array() {
                    walk(children, &format!("{here}.tabs[{t}].children"), seen, problems);
                }
            }
        }
    }
    let Some(blocks) = blocks.as_array() else { return vec!["blocks must be an array".into()] };
    let (mut seen, mut problems) = (Vec::new(), Vec::new());
    walk(blocks, "blocks", &mut seen, &mut problems);
    problems
}

/// A pane's patch, as the node applies it: RFC 6902's add, replace and remove (a patch with any other
/// op is dropped whole). The content with every operation applied, or why it can't be. Paths go
/// through ids, `/b/<block id>/…` or `/i/<item id>/…`, and `/b/<id>` alone is the block itself:
/// replace swaps it, remove takes it out, add puts one before it.
pub fn apply_patch(content: &Value, ops: &Value) -> Result<Value, String> {
    let mut out = content.clone();
    for op in ops.as_array().into_iter().flatten() {
        let path = op["path"].as_str().unwrap_or_default();
        match op["op"].as_str().unwrap_or_default() {
            "add" => add(&mut out, path, op["value"].clone())?,
            "remove" => drop(remove(&mut out, path)?),
            "replace" => {
                let at = locate(&out, path)?;
                let there = get_mut(&mut out, &at).ok_or_else(|| nothing(path, at.last()))?;
                *there = op["value"].clone();
            }
            other => {
                return Err(format!(
                    "'{other}' isn't one of add, replace and remove, which is all a pane's patch takes"
                ))
            }
        }
    }
    Ok(out)
}

/// A patch path as plain tokens from the content's root, through the block or item it names, or a plain
/// pointer from the top of the content as a set sends it (`/blocks/…`, `/board/…`).
fn locate(content: &Value, path: &str) -> Result<Vec<String>, String> {
    let tokens: Vec<String> = match path.strip_prefix('/') {
        Some(rest) => rest.split('/').map(|t| t.replace("~1", "/").replace("~0", "~")).collect(),
        None => Vec::new(),
    };
    let top = if content.is_array() { "blocks" } else { "board" };
    if tokens.len() >= 2 && tokens[0] == top {
        return Ok(tokens[1..].to_vec());
    }
    if tokens.len() < 2 || (tokens[0] != "b" && tokens[0] != "i") {
        return Err(format!(
            "'{path}': a pane's patch paths start /b/<block id> or /i/<item id>, or /{top}/ from the top"
        ));
    }
    let base = if tokens[0] == "b" {
        content.as_array().and_then(|blocks| find_block(blocks, &tokens[1]))
    } else {
        content["items"]
            .as_array()
            .and_then(|items| items.iter().position(|item| item["id"] == tokens[1].as_str()))
            .map(|i| vec!["items".to_string(), i.to_string()])
    };
    let what = if tokens[0] == "b" { "block" } else { "item" };
    let mut at = base.ok_or_else(|| format!("'{path}': no {what} '{}'", tokens[1]))?;
    at.extend(tokens[2..].iter().cloned());
    Ok(at)
}

fn find_block(blocks: &[Value], id: &str) -> Option<Vec<String>> {
    for (i, block) in blocks.iter().enumerate() {
        if block["id"] == id {
            return Some(vec![i.to_string()]);
        }
        let mut lists: Vec<(Vec<String>, &Vec<Value>)> = Vec::new();
        if let Some(children) = block["children"].as_array() {
            lists.push((vec![i.to_string(), "children".into()], children));
        }
        for (t, tab) in block["tabs"].as_array().into_iter().flatten().enumerate() {
            if let Some(children) = tab["children"].as_array() {
                lists.push((vec![i.to_string(), "tabs".into(), t.to_string(), "children".into()], children));
            }
        }
        for (mut at, children) in lists {
            if let Some(rest) = find_block(children, id) {
                at.extend(rest);
                return Some(at);
            }
        }
    }
    None
}

fn get<'a>(v: &'a Value, at: &[String]) -> Option<&'a Value> {
    at.iter().try_fold(v, |v, token| match v {
        Value::Object(map) => map.get(token),
        Value::Array(items) => items.get(token.parse::<usize>().ok()?),
        _ => None,
    })
}

fn get_mut<'a>(v: &'a mut Value, at: &[String]) -> Option<&'a mut Value> {
    at.iter().try_fold(v, |v, token| match v {
        Value::Object(map) => map.get_mut(token),
        Value::Array(items) => items.get_mut(token.parse::<usize>().ok()?),
        _ => None,
    })
}

fn nothing(path: &str, key: Option<&String>) -> String {
    format!("'{path}': nothing at '{}'", key.map(String::as_str).unwrap_or_default())
}

fn add(content: &mut Value, path: &str, value: Value) -> Result<(), String> {
    let at = locate(content, path)?;
    let (key, parent) = at.split_last().ok_or_else(|| nothing(path, None))?;
    let parent = get_mut(content, parent).ok_or_else(|| nothing(path, parent.last()))?;
    match parent {
        Value::Object(map) => {
            map.insert(key.clone(), value);
        }
        Value::Array(items) if key == "-" => items.push(value),
        Value::Array(items) => {
            let i: usize = key.parse().map_err(|_| format!("'{path}': '{key}' isn't an index"))?;
            if i > items.len() {
                return Err(format!("'{path}': {i} is past the end"));
            }
            items.insert(i, value);
        }
        _ => {
            return Err(format!("'{path}': can't add inside a {}", if parent.is_string() { "string" } else { "value" }))
        }
    }
    Ok(())
}

fn remove(content: &mut Value, path: &str) -> Result<Value, String> {
    let at = locate(content, path)?;
    let (key, parent) = at.split_last().ok_or_else(|| nothing(path, None))?;
    let parent = get_mut(content, parent).ok_or_else(|| nothing(path, parent.last()))?;
    let removed = match parent {
        Value::Object(map) => map.remove(key),
        Value::Array(items) => key.parse::<usize>().ok().filter(|i| *i < items.len()).map(|i| items.remove(i)),
        _ => None,
    };
    removed.ok_or_else(|| nothing(path, Some(key)))
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
    use super::{allows_secret, apply_patch, block_problems, board_problems};
    use crate::pointer;
    use serde_json::{json, Value};

    #[test]
    fn patches_go_through_ids() {
        let blocks = json!([
            {"type": "tabs", "id": "t", "tabs": [
                {"id": "one", "title": "One", "children": [{"type": "list", "id": "a/b", "items": []}]}]},
            {"type": "text", "id": "x", "markdown": "hi"}]);
        let out = apply_patch(
            &blocks,
            &json!([
                {"op": "add", "path": pointer(["b", "a/b", "items", "-"]), "value": {"id": "r1", "title": "Row"}},
                {"op": "add", "path": "/b/a~1b/items/0", "value": {"id": "r0", "title": "First"}},
                {"op": "add", "path": "/b/a~1b/note", "value": "hi"},
                {"op": "replace", "path": "/b/x/markdown", "value": "bye"},
                {"op": "remove", "path": "/b/t/tabs/0/children/0/items/1"},
            ]),
        )
        .unwrap();
        let inner = &out[0]["tabs"][0]["children"][0];
        assert_eq!(inner["items"], json!([{"id": "r0", "title": "First"}]));
        assert_eq!(inner["note"], "hi");
        assert_eq!(out[1], json!({"type": "text", "id": "x", "markdown": "bye"}));
        // /b/<id> alone is the block: remove takes it out, replace swaps it, add puts one before it
        assert_eq!(apply_patch(&blocks, &json!([{"op": "remove", "path": "/b/x"}])).unwrap(), json!([blocks[0]]));
        let swapped =
            apply_patch(&blocks, &json!([{"op": "replace", "path": "/b/x", "value": {"type": "text", "id": "z"}}]));
        assert_eq!(swapped.unwrap()[1]["id"], "z");
        let before =
            apply_patch(&blocks, &json!([{"op": "add", "path": "/b/x", "value": {"type": "text", "id": "w"}}]));
        let ids: Vec<Value> = before.unwrap().as_array().unwrap().iter().map(|b| b["id"].clone()).collect();
        assert_eq!(ids, [json!("t"), json!("w"), json!("x")]);
        let board = json!({"items": [{"id": "i1", "state": "Todo"}]});
        let moved = apply_patch(&board, &json!([{"op": "replace", "path": "/i/i1/state", "value": "Done"}])).unwrap();
        assert_eq!(moved["items"][0]["state"], "Done");
        // plain pointers from the top of the content, as a set sends it: {"blocks": […]} or {"board": {…}}
        let grown =
            apply_patch(&board, &json!([{"op": "add", "path": "/board/items/-", "value": {"id": "i2"}}])).unwrap();
        assert_eq!(grown["items"][1]["id"], "i2");
        let more =
            apply_patch(&blocks, &json!([{"op": "add", "path": "/blocks/-", "value": {"type": "text", "id": "y"}}]));
        assert_eq!(more.unwrap()[2]["id"], "y");
        assert!(apply_patch(&blocks, &json!([{"op": "add", "path": "/board/items/-", "value": {}}])).is_err());
        for (bad, why) in [
            (json!({"op": "replace", "path": "/0/markdown", "value": 1}), "start /b/<block id>"),
            (json!({"op": "replace", "path": "/b/nope/x", "value": 1}), "no block 'nope'"),
            (json!({"op": "replace", "path": "/b/x/tone", "value": 1}), "nothing at 'tone'"),
            (json!({"op": "add", "path": "/b/a~1b/items/5", "value": 1}), "past the end"),
            (json!({"op": "move", "from": "/b/x/markdown", "path": "/b/x/text"}), "isn't one of add"),
            (json!({"op": "test", "path": "/b/x/markdown", "value": "hi"}), "isn't one of add"),
        ] {
            let e = apply_patch(&blocks, &json!([bad])).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
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
    fn blocks_need_a_type_and_an_id_once() {
        let blocks = json!([{"type": "text", "id": "a"}, {"type": "stack", "id": "s", "children": [
            {"type": "text", "id": "a"}, {"id": "n"}, {"type": "stat"}]}]);
        assert_eq!(
            block_problems(&blocks),
            [
                "blocks[1].children[0]: the id 'a' is used twice",
                "blocks[1].children[1] has no type",
                "blocks[1].children[2] (stat) has no id",
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
