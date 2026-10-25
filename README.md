# pointman-extension

The Rust SDK for Pointman node extensions: the extension side of the node's protocol
(newline-delimited JSON-RPC 2.0 on stdin and stdout, the log on stderr), message for message as the
Python SDK speaks it, through buffa's types for the protocol's messages, and a stand-in node for an extension's own tests. Std threads and serde_json,
no async runtime. MIT.

```toml
[dependencies]
pointman-extension = { git = "https://github.com/pointman-ai/pointman-extension", branch = "types" }
```

## The protocol, as Rust types

The protocol is Pointman's `proto/pointman/extension/v1/protocol.proto` (TYPES-9): JSON-RPC 2.0 over stdio as ever, and each
method's params and result are that method's messages, in protobuf's standard JSON. buffa builds them (build.rs, from
`proto/descriptors.pb`, with no protoc); `pointman_extension::protocol` has them (`ProgressRequest`,
`PaneOpenResponse`…), and `pointman_extension::v1` the blocks, pane content, edits and ids they carry
(`Block`, `PaneContent`, `PaneEdit`, `PaneInput`).

- A method's name is its rpc's: `PaneOpen` is `pane.open`, `CredentialsSignIn` is `credentials.sign_in`. An rpc that
  returns `Empty` is a notification (`Notification`); the rest are requests (`Request`, with their `Response`).
  `pointman_extension::METHODS` lists all 58.
- Lists stay open: an unknown method, field or enum value is skipped, never refused. A request for a method the SDK
  doesn't know is answered with an error; a notification is dropped.
- An extension's own data (its settings, a command's params and result, its events, a board's items) is free-form in
  the protocol (`Struct`, `Value`), and `serde_json::Value` here.
- `proto/descriptors.pb` is core's, trimmed to protocol.proto's closure (`proto/SOURCE` says which commit);
  `cargo run --example trim_descriptors -- <core>/src/pointman/descriptors.pb proto/descriptors.pb <rev>` makes it.

## The API

```rust
use pointman_extension::protocol::{output_request::Role, HealthCheckRequest, HealthCheckResponse};
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
        job.output(job.work.join("out.txt"), "document", Role::Main, json!({}));
        job.check()?; // stops here if cancelled (job.cancelled() to ask)
        Err(Retry::new("no account free").into()) // failed, retry: true; any other error, retry: false
    });
    // any other request of the node's, by its message: the answer is the response's message
    ext.on(|_: HealthCheckRequest| Ok(HealthCheckResponse::default()));
    ext.on_start(|h: &Handle| h.log("started"));
    ext.every(Duration::from_secs(60), |h: &Handle| h.event("rota.accounts", json!({})), "rota");

    h.event("rota.accounts", json!({"n": 2})); // sent only when it changed
    let names = h.secrets("rota.")?; // SecretListRequest; waits up to 30 s
    let token = h.secret("rota.work")?; // SecretGetRequest
    // any request to the node by its message: h.request(&BoardPostRequest { .. })? -> BoardPostResponse
    // any notification: h.notify(&PaneTitleRequest { .. })
    let (settings, node) = (h.settings(), h.node()); // node: the InitializeRequest (node, machine, work_dir, state_dir, roots)
    let path = h.allowed("/some/path")?; // inside the node's roots, or an error
    ext.run() // until shutdown, or stdin closes
}
```

Commands in the same lane (`lane` in `[[commands]]`) run one at a time on the lane's thread, with
its `every` tasks between them; a command with no lane, and each `on` request, gets a thread of
its own. Settings get the defaults from the manifest's `[settings]` schema (a change restarts the extension: there's no
`settings.changed`). Callbacks may return `()` or a `Result`; a panic in one fails that command (or is logged) and the
extension carries on. Numbers in free-form data come back as integers when they are whole (protobuf's `Value` has one
kind of number).

## Panes

A pane shows something an extension keeps on the node (Pointman's docs/dev/panes.md): blocks, a
tracker's board, a page, or video. Declare each one in extension.toml, answer its open, then keep it current:

```toml
[[panes]]
id = "view"
title = "GitHub"
format = "blocks"                        # or "board", "page" or "video"
opens = ["link", "item", "command"]
```

```rust
use pointman_extension::blocks::{heading, stat, text};
use pointman_extension::protocol::PaneOpenResponse;
use pointman_extension::v1::{PaneContext, PaneInput, Tone};
use pointman_extension::{Pane, Refused};

ext.pane("view", |pane: &Pane| {
    // pane.id(), pane.open(), pane.target() (Value), pane.context() (PaneContext), pane.viewer()
    pane.on_input(|pane: &Pane, event: &PaneInput| -> anyhow::Result<()> {
        if event.str("block") == Some("merge") {
            return Err(Refused::new("CI hasn't passed").into()); // the app snaps back and says why
        }
        pane.show(blocks("merged")); // only the block that changed goes: an edit by its name
        Ok(())
    });
    pane.on_context(|pane: &Pane, context: &PaneContext| { /* the thread or item the person is on */ });
    pane.on_close(|pane: &Pane, reason: &str| { /* stop what feeds it */ });
    // or PaneOpenResponse::board(&board), ::page("index.html"), ::video(Video { .. })
    Ok(PaneOpenResponse::blocks(blocks("open")).titled("PR 42"))
});
// later, from anywhere: pane.show(blocks) or pane.show_board(&board) sends what changed since the last;
// also pane.set(blocks), pane.set_board(&board), pane.edit(edits), pane.replace(block), pane.put_board_item(&item),
// pane.set_title(t), pane.close(reason); h.panes("view") lists the open ones
```

Every block has a `name`, unique in the pane (`pointman_extension::blocks` builds the usual ones). The format comes from
the manifest. To keep a pane current, build all of its content each time and `show` it: `edits::diff` sends a block that
changed as a replace, a list's or table's or waterfall's new and changed items as `put_items` by id (and its own
fields alone, with `keep_items`, if those changed too), and blocks that came, went or moved as inserts, removes and
moves; which tab is open is the person's, so `selected` never goes. A board's items go by id too. When the changes
can't be said in edits, all of it goes. The node applies edits in order, all or none (`edits::apply` is how); the
fixtures under tests/fixtures hold the cases every side checks.

Each pane's open, inputs, context and close run in order on a thread of its own, and what it sends while opening goes
after the open's answer. examples/panes has one of each kind, examples/video a video pane.

### Video

A `video` pane opens with `PaneOpenResponse::video(Video { codec, width, height, fps, audio })`, sends frames with
`pane.frame(jpeg, width, height, captured_ms)` and AAC sound with `pane.audio(aac, captured_ms)`, hears how many of the
app's paths are attached with `pane.on_viewers`, and takes the app's hints as inputs (`kind: "hint"`, `max_fps`,
`max_width`, `audio`). `pane.on_control` hears a person being given control (or not any longer), and refuses it with
a `Refused`. Frames are lossy and newest-wins: one before the open is answered, after the close, or over 4 MiB sealed is
dropped (`frame` says false).

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
    .on(|_: OauthLoadRequest| Ok(OauthLoadResponse::default())) // more the extension may ask the node, by message
    .method("oauth.load", |p| Ok(json!({})))            // or by name, with JSON
    .start()?;                                          // stops when dropped, or .stop()
let done = node.run("rota.swap", json!({}))?;           // .result .progress .outputs .checkpoints (the messages it sent)
node.run_with("rota.swap", json!({}), RunOptions { cid: Some("c1".into()), ..Default::default() })?;
node.call("health.check", json!({}))?;                   // its own methods, params and result as JSON; call_timeout, run_timeout
node.events()["rota.accounts"];                          // also wait_event, asked, log, cancel (whether it was running it)
let view = node.open_pane("view", json!({"url": "…"}))?; // also open_pane_with(…, PaneOptions { context, .. })
view.input_json(json!({"kind": "tap", "block": "merge"}))?; // or input(PaneInput); Err(Failed) with its reason if refused
view.block("status");                                    // blocks, board, item, title, format, path, closed
view.look_at(PaneContext { .. })?; view.close("closed")?; // wait(timeout, test), frames, problems
```

A pane's view keeps its content as an app would, with every set and edit applied (edits as the node applies them,
all or none); `problems()` lists what the node would refuse (edits that don't apply, a set over 1 MiB, a block with
no name or a family, or a name used twice). A video pane's view keeps its newest frames and sound
(`video_frames`, `sounds`, `frame_count`, `sound_count`), says how many paths are attached (`viewers(n)`), and sends
hints (`hint`, `audio_hint`) and a person's control (`control`, `control_input`).

A failure is a `testing::Failed { message, retry, cancelled }`, whose Display is the extension's own
message.

This crate's own tests: `cargo test`. They run `examples/hello` (the Rust twin of the Python
template's `hello.greet`), `examples/demo` (every part of the API), `examples/panes` and `examples/video` under
`StandIn`, drive the demo line by line to check the wire format, read every protocol fixture (tests/fixtures/extension)
back and forth, and apply the shared edit cases (tests/fixtures/panes/edits.json). `tests/python.rs` also runs core's
Python node and template against these, when `POINTMAN_CORE` points at a checkout of Pointman's core
(`POINTMAN_CORE_PYTHON`: a Python with its dependencies, if not its `.venv`). Without it, as on CI, those two are
skipped.

The protocol and the manifest are documented in Pointman's docs/dev/extensions.md.
