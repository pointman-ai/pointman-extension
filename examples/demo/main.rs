//! Every part of the SDK in one extension, for the SDK's own tests: commands with and without a
//! lane, progress, checkpoints, outputs, events, retry, cancel, secrets, the node's own requests
//! (`mcp.call_tool`, standing in for any), requests to the node, settings and a periodic task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use pointman_extension::protocol::output_request::Role;
use pointman_extension::protocol::{
    FilesGetRequest, FilesListRequest, FilesPutRequest, McpCallToolRequest, McpCallToolResponse,
};
use pointman_extension::{convert, Extension, Handle, Job, Retry};
use serde_json::{json, Value};

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()
}

fn thread_name() -> String {
    thread::current().name().unwrap_or_default().to_string()
}

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let h = ext.handle();
    let ticks = Arc::new(AtomicU64::new(0));

    // no lane: its params back (after a pause, if asked), with progress, a checkpoint, an output
    // and an event, sent twice to show it goes once
    let echo = h.clone();
    ext.command("demo.echo", move |job: &Job| {
        let started = now_ms();
        if let Some(ms) = job.params["sleep_ms"].as_u64() {
            thread::sleep(Duration::from_millis(ms));
        }
        job.progress(0.5, "echoing");
        job.progress_detail(1.0, None, json!({"items": 1}));
        job.save(json!({"echoed": true}));
        let out = job.work.join("echo.json");
        std::fs::write(&out, job.params.to_string())?;
        job.output(&out, "document", Role::Main, Value::Null);
        echo.event("demo.echo", json!({"last": job.params}));
        echo.event("demo.echo", json!({"last": job.params}));
        Ok(json!({
            "params": job.params, "thread": thread_name(), "checkpoint": job.checkpoint,
            "work": job.work, "started": started, "ended": now_ms(),
        }))
    });

    // lane "main": counts to `steps`, from the checkpoint when resuming, saving one each step; the
    // lane's periodic task doesn't run while it does
    let during = ticks.clone();
    ext.command("demo.step", move |job: &Job| {
        let (started, ticks_before) = (now_ms(), during.load(Ordering::SeqCst));
        let steps = job.params["steps"].as_u64().unwrap_or(3);
        let pause = Duration::from_millis(job.params["step_ms"].as_u64().unwrap_or(20));
        let from = job.checkpoint.as_ref().and_then(|c| c["step"].as_u64()).unwrap_or(0);
        for step in from..steps {
            job.check()?;
            job.progress(step as f64 / steps as f64, "stepping");
            thread::sleep(pause);
            job.save(json!({"step": step + 1}));
        }
        Ok(json!({
            "from": from, "to": steps, "thread": thread_name(), "started": started, "ended": now_ms(),
            "ticks_during": during.load(Ordering::SeqCst) - ticks_before,
        }))
    });

    let fails = h.clone();
    ext.command("demo.fail", move |job: &Job| -> anyhow::Result<Value> {
        let message = job.params["message"].as_str().unwrap_or("it broke");
        match job.params["how"].as_str() {
            Some("retry") => Err(Retry::new(message).into()),
            Some("wrapped") => Err(anyhow::Error::from(Retry::new(message)).context("while asking")),
            Some("panic") => panic!("{message}"),
            Some("event") => {
                fails.event("other.thing", json!({}));
                Ok(Value::Null)
            }
            Some("exit") => std::process::exit(3),
            _ => bail!("{message}"),
        }
    });

    let vault = h.clone();
    ext.command("demo.secret", move |job: &Job| {
        let mut out = json!({});
        if let Some(name) = job.params["name"].as_str() {
            out["value"] = json!(vault.secret(name)?);
        }
        if let Some(prefix) = job.params["prefix"].as_str() {
            out["names"] = json!(vault.secrets(prefix)?);
        }
        Ok(out)
    });

    let paths = h.clone();
    ext.command("demo.allowed", move |job: &Job| {
        Ok(json!({"path": paths.allowed(job.params["path"].as_str().unwrap_or_default())?}))
    });

    // the node's own requests, beyond commands: a tool call, by tool name, `ask` or `relay`
    let asked = h.clone();
    ext.on(move |call: McpCallToolRequest| {
        let args = convert::from_struct(&call.arguments);
        let result = match call.name.as_str() {
            "ask" => {
                if args["fail"] == true {
                    bail!("asked to fail");
                }
                if let Some(ms) = args["sleep_ms"].as_u64() {
                    thread::sleep(Duration::from_millis(ms));
                }
                json!({"params": args, "settings": asked.settings(), "thread": thread_name()})
            }
            // a request to the node: its answer back, or its reason
            "relay" => match args["method"].as_str().unwrap_or_default() {
                "files.get" => {
                    let path = args["path"].as_str().unwrap_or_default().to_string();
                    let got = asked.request(&FilesGetRequest { path, ..Default::default() })?;
                    serde_json::to_value(got)?
                }
                "files.put" => {
                    let (path, text) = (args["path"].as_str(), args["text"].as_str());
                    let put = FilesPutRequest {
                        path: path.unwrap_or_default().into(),
                        text: text.unwrap_or_default().into(),
                        ..Default::default()
                    };
                    serde_json::to_value(asked.request(&put)?)?
                }
                _ => serde_json::to_value(asked.request(&FilesListRequest::default())?)?,
            },
            other => bail!("no tool {other}"),
        };
        Ok(McpCallToolResponse { result: convert::to_struct(&result).into(), ..Default::default() })
    });

    ext.on_start(|h: &Handle| h.event("demo.started", json!({"node": h.node(), "settings": h.settings()})));
    ext.every(
        Duration::from_millis(50),
        move |h: &Handle| h.event("demo.tick", json!({"ticks": ticks.fetch_add(1, Ordering::SeqCst) + 1})),
        "main",
    );

    ext.run()
}
