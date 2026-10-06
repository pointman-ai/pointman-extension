//! The wire itself: the demo example driven line by line, checking each message is the one the
//! Python SDK sends (docs/dev/extensions.md, "Running an extension").

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use common::{example, package, source, tmp};
use serde_json::{json, Value};

struct Wire {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    seen: Vec<Value>,
    stderr: Receiver<String>,
}

impl Wire {
    fn start(binary: &Path, cwd: &Path, env: &[(&str, &Path)]) -> Wire {
        let mut cmd = Command::new(binary);
        cmd.current_dir(cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        cmd.env_remove(pointman_extension::MANIFEST_ENV);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let (tx, lines) = mpsc::channel();
        let stdout = child.stdout.take().unwrap();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let msg: Value = serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON on stdout: {line}"));
                if tx.send(msg).is_err() {
                    break;
                }
            }
        });
        let (etx, stderr) = mpsc::channel();
        let mut err = child.stderr.take().unwrap();
        thread::spawn(move || {
            let mut text = String::new();
            let _ = err.read_to_string(&mut text);
            let _ = etx.send(text);
        });
        Wire { child, stdin, lines, seen: Vec::new(), stderr }
    }

    fn send(&mut self, msg: Value) {
        self.raw(&msg.to_string());
    }

    fn raw(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next message (skipping the periodic task's events) that passes `test`; what's skipped
    /// stays in `seen`.
    fn next(&mut self, test: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = self.lines.recv_timeout(left).unwrap_or_else(|_| panic!("nothing more; saw {:#?}", self.seen));
            if test(&msg) {
                return msg;
            }
            self.seen.push(msg);
        }
    }

    fn answer(&mut self, id: u64) -> Value {
        self.next(|m| m["id"] == id && m.get("method").is_none())
    }

    fn notes(&mut self, cid: &str, until: &str) -> Vec<Value> {
        let mut notes = Vec::new();
        loop {
            let msg = self.next(|m| m["params"]["id"] == cid && m.get("id").is_none());
            let done = msg["method"] == until;
            notes.push(msg);
            if done {
                return notes;
            }
        }
    }

    fn initialize(&mut self, work: &Path) {
        self.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocol": 1, "node": "n", "machine": "m", "settings": {"label": "x"},
            "work_dir": work, "state_dir": work, "roots": [],
        }}));
        assert_eq!(
            self.answer(1),
            json!({"jsonrpc": "2.0", "id": 1, "result": {"protocol": 1, "id": "demo", "version": "0.2.0"}})
        );
    }

    fn exit(mut self) -> (i32, String) {
        drop(self.stdin.take());
        let code = self.child.wait().unwrap().code().unwrap_or(-1);
        (code, self.stderr.recv_timeout(Duration::from_secs(5)).unwrap_or_default())
    }
}

fn wire() -> (Wire, std::path::PathBuf) {
    let work = tmp("wire");
    let mut w = Wire::start(&example("demo"), &source("demo"), &[]);
    w.initialize(&work);
    (w, work)
}

#[test]
fn a_command_on_the_wire() {
    let (mut w, work) = wire();
    let start =
        json!({"id": "c1", "kind": "demo.echo", "params": {"a": 1}, "work": work.join("c1"), "checkpoint": null});
    w.send(json!({"jsonrpc": "2.0", "id": 2, "method": "command.start", "params": start}));
    assert_eq!(w.answer(2), json!({"jsonrpc": "2.0", "id": 2, "result": {"accepted": true}}));
    let notes = w.notes("c1", "done");
    let out = work.join("c1/echo.json").to_string_lossy().into_owned();
    assert_eq!(
        notes[..4],
        [
            json!({"jsonrpc": "2.0", "method": "progress", "params": {"id": "c1", "fraction": 0.5, "stage": "echoing", "detail": {}}}),
            json!({"jsonrpc": "2.0", "method": "progress", "params": {"id": "c1", "fraction": 1.0, "stage": null, "detail": {"items": 1}}}),
            json!({"jsonrpc": "2.0", "method": "checkpoint", "params": {"id": "c1", "state": {"echoed": true}}}),
            json!({"jsonrpc": "2.0", "method": "output", "params": {"id": "c1", "path": out, "kind": "document", "role": "main", "meta": {}}}),
        ]
    );
    let done = notes.last().unwrap();
    assert_eq!(done["method"], "done");
    assert_eq!(done["params"]["result"]["params"], json!({"a": 1}));
    // the event went once, though sent twice
    let echoes: Vec<&Value> = w.seen.iter().filter(|m| m["params"]["name"] == "demo.echo").collect();
    assert_eq!(
        echoes,
        [&json!({"jsonrpc": "2.0", "method": "event", "params": {"name": "demo.echo", "data": {"last": {"a": 1}}}})]
    );
    w.send(json!({"jsonrpc": "2.0", "id": 3, "method": "shutdown"}));
    assert_eq!(w.answer(3), json!({"jsonrpc": "2.0", "id": 3, "result": {}}));
    assert_eq!(w.exit().0, 0);
}

#[test]
fn the_same_id_again_is_the_same_command() {
    let (mut w, work) = wire();
    let start = json!({"id": 7, "kind": "demo.step", "params": {"steps": 10, "step_ms": 30}, "work": work.join("7")});
    w.send(json!({"jsonrpc": "2.0", "id": 2, "method": "command.start", "params": start}));
    assert_eq!(w.answer(2)["result"], json!({"accepted": true}));
    w.send(json!({"jsonrpc": "2.0", "id": 3, "method": "command.start", "params": start}));
    assert_eq!(w.answer(3)["result"], json!({"accepted": true, "already": true}));
    w.send(json!({"jsonrpc": "2.0", "id": 4, "method": "command.cancel", "params": {"id": 7}}));
    assert_eq!(w.answer(4)["result"], json!({"found": true}));
    let failed = w.notes("7", "failed").pop().unwrap();
    assert_eq!(failed["params"], json!({"id": "7", "error": "cancelled", "retry": false, "cancelled": true}));
    w.send(json!({"jsonrpc": "2.0", "id": 5, "method": "command.cancel", "params": {"id": "nope"}}));
    assert_eq!(w.answer(5)["result"], json!({"found": false}));
}

#[test]
fn failures_on_the_wire() {
    let (mut w, work) = wire();
    let start =
        |id: &str, params: Value| json!({"id": id, "kind": "demo.fail", "params": params, "work": work.join(id)});
    w.send(json!({"jsonrpc": "2.0", "id": 2, "method": "command.start", "params": start("r", json!({"how": "retry", "message": "later"}))}));
    assert_eq!(w.notes("r", "failed").pop().unwrap()["params"], json!({"id": "r", "error": "later", "retry": true}));
    w.send(
        json!({"jsonrpc": "2.0", "id": 3, "method": "command.start", "params": start("f", json!({"message": "no"}))}),
    );
    assert_eq!(w.notes("f", "failed").pop().unwrap()["params"], json!({"id": "f", "error": "no", "retry": false}));

    w.send(json!({"jsonrpc": "2.0", "id": 4, "method": "command.start", "params": {"id": "u", "kind": "demo.nope"}}));
    assert_eq!(w.answer(4)["error"], json!({"code": -32000, "message": "demo.nope isn't a command of demo"}));
    w.send(json!({"jsonrpc": "2.0", "id": 5, "method": "no.such"}));
    assert_eq!(
        w.answer(5),
        json!({"jsonrpc": "2.0", "id": 5, "error": {"code": -32601, "message": "no method no.such"}})
    );
    w.send(json!({"jsonrpc": "2.0", "method": "no.such"})); // a notification: no answer
    w.raw("this isn't JSON");
    w.raw("");
    w.send(json!({"jsonrpc": "2.0", "id": 6, "method": "demo.ask", "params": {"fail": true}}));
    assert_eq!(w.answer(6)["error"], json!({"code": -32000, "message": "asked to fail"}));
    let (code, log) = w.exit(); // stdin closed: it exits
    assert_eq!(code, 0);
    assert!(log.contains("not JSON: this isn't JSON"), "{log}");
}

#[test]
fn its_requests_to_the_node_have_ids_and_get_answers() {
    let (mut w, work) = wire();
    let start = json!({"id": "s", "kind": "demo.secret", "params": {"name": "demo.work", "prefix": "demo."}, "work": work.join("s")});
    w.send(json!({"jsonrpc": "2.0", "id": 2, "method": "command.start", "params": start}));
    let ask = w.next(|m| m["method"] == "secret.get");
    assert_eq!(ask, json!({"jsonrpc": "2.0", "id": "x1", "method": "secret.get", "params": {"name": "demo.work"}}));
    w.send(json!({"jsonrpc": "2.0", "id": "x1", "result": {"value": "w"}}));
    let list = w.next(|m| m["method"] == "secret.list");
    assert_eq!(list["id"], "x2");
    w.send(json!({"jsonrpc": "2.0", "id": "x2", "result": {"names": ["demo.work"]}}));
    let done = w.notes("s", "done").pop().unwrap();
    assert_eq!(done["params"]["result"], json!({"value": "w", "names": ["demo.work"]}));

    let refused = json!({"id": "t", "kind": "demo.secret", "params": {"name": "demo.x"}, "work": work.join("t")});
    w.send(json!({"jsonrpc": "2.0", "id": 3, "method": "command.start", "params": refused}));
    let ask = w.next(|m| m["method"] == "secret.get");
    w.send(json!({"jsonrpc": "2.0", "id": ask["id"], "error": {"code": -32000, "message": "not in its permissions"}}));
    assert_eq!(w.notes("t", "failed").pop().unwrap()["params"]["error"], "not in its permissions");
}

#[test]
fn it_finds_its_manifest_beside_its_bin_folder_or_from_the_env() {
    let root = package("demo");
    let elsewhere = tmp("elsewhere");
    let mut w = Wire::start(&root.join("bin/demo"), &elsewhere, &[]);
    w.initialize(&elsewhere);
    w.exit();

    let mut w = Wire::start(&example("demo"), &elsewhere, &[(pointman_extension::MANIFEST_ENV, &source("demo"))]);
    w.initialize(&elsewhere);
    w.exit();

    let w = Wire::start(&example("demo"), &elsewhere, &[]);
    let (code, log) = w.exit();
    assert_ne!(code, 0);
    assert!(log.contains("no extension.toml"), "{log}");
    std::fs::remove_dir_all(root).unwrap();
}
