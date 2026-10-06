# pointman-extension

The Rust SDK for Pointman node extensions: the extension side of the node's protocol
(newline-delimited JSON-RPC 2.0 on stdin and stdout, the log on stderr), message for message as the
Python SDK speaks it, and a stand-in node for an extension's own tests. Std threads and serde_json,
no async runtime. MIT.

```toml
[dependencies]
pointman-extension = { git = "https://github.com/pointman-ai/pointman-extension" }
```

## The API

```rust
use pointman_extension::{Extension, Handle, Job, Retry};
use serde_json::{json, Value};
use std::time::Duration;

fn main() -> anyhow::Result<()> {
    // extension.toml: $POINTMAN_EXTENSION_MANIFEST, else the working folder, else the binary's ../
    let mut ext = Extension::new()?;
    let h: Handle = ext.handle(); // Clone + Send + Sync, usable from any thread

    ext.command("rota.swap", |job: &Job| -> anyhow::Result<Value> {
        // job.id, job.kind, job.params (Value), job.work (PathBuf), job.checkpoint (Option<Value>)
        job.progress(0.5, "swapping"); // or progress_detail(fraction, stage, json!({..}))
        job.save(json!({"step": 2})); // a checkpoint, given back as job.checkpoint on a restart
        job.output(job.work.join("out.txt"), "document", "main", json!({}));
        job.check()?; // stops here if cancelled (job.cancelled() to ask)
        Err(Retry::new("no account free").into()) // failed, retry: true; any other error, retry: false
    });
    ext.method("session.prepare", |params: Value| Ok(json!({"env": {}}))); // any other node request
    ext.on_start(|h: &Handle| h.log("started"));
    ext.on_settings(|h: &Handle| h.log(format!("settings: {}", h.settings())));
    ext.every(Duration::from_secs(60), |h: &Handle| h.event("rota.accounts", json!({})), "rota");

    h.event("rota.accounts", json!({"n": 2})); // sent only when it changed
    let names = h.request("secret.list", json!({"prefix": "rota."}))?; // waits up to 30 s
    let token = h.secret("rota.work")?;
    let (settings, node) = (h.settings(), h.node()); // node: initialize's protocol, node, machine, work_dir, state_dir, roots
    let path = h.allowed("/some/path")?; // inside the node's roots, or an error
    ext.run() // until shutdown, or stdin closes
}
```

Commands in the same lane (`lane` in `[[commands]]`) run one at a time on the lane's thread, with
its `every` tasks between them; a command with no lane, and each `method` call, gets a thread of
its own. Settings get the defaults from the manifest's `[settings]` schema. Callbacks may return
`()` or a `Result`; a panic in one fails that command (or is logged) and the extension carries on.

## Testing

`pointman_extension::testing::StandIn` starts the extension as a node does (its `[run]` command,
from its folder) and answers `secret.get` and `secret.list` from fakes, only within
`permissions.secrets`:

```rust
use pointman_extension::testing::{RunOptions, StandIn};

let node = StandIn::new(env!("CARGO_MANIFEST_DIR"))     // the folder with extension.toml
    .binary(env!("CARGO_BIN_EXE_rota"))                 // replaces [run]'s first word
    .settings(json!({"pick": "most_left"}))
    .secrets([("rota.work", "token")])
    .env("PATH", path).machine("test-machine")          // also .node, .state_dir, .roots
    .method("oauth.load", |p| Ok(json!({})))            // more the extension may ask the node
    .start()?;                                          // stops when dropped, or .stop()
let done = node.run("rota.swap", json!({}))?;           // .result .progress .outputs .checkpoints
node.run_with("rota.swap", json!({}), RunOptions { cid: Some("c1".into()), ..Default::default() })?;
node.call("session.prepare", json!({}))?;                // its own methods; call_timeout, run_timeout
node.events()["rota.accounts"];                          // also wait_event, asked, log, cancel, change_settings
```

A failure is a `testing::Failed { message, retry, cancelled }`, whose Display is the extension's own
message.

This crate's own tests: `cargo test`. They run `examples/hello` (the Rust twin of the Python
template's `hello.greet`) and `examples/demo` (every part of the API) under `StandIn`, and drive the
demo line by line to check the wire format. `tests/python.rs` also runs both under core's Python node
(`ExtensionHost` and its stand-in), and core's Python template under `StandIn`, when
`POINTMAN_CORE` points at a checkout of Pointman's core (`POINTMAN_CORE_PYTHON`: a Python with its
dependencies, if not its `.venv`). Without it, as on CI, those two are skipped.

The protocol and the manifest are documented in Pointman's docs/dev/extensions.md.
