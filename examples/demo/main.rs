//! Every part of the SDK in one extension, for the SDK's own tests: commands with and without a
//! lane, progress, checkpoints, outputs, events, retry, cancel, secrets, the node's own requests,
//! requests to the node, settings and a periodic task.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use pointman_extension::{Extension, Handle, Job, Retry};
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
        job.output(&out, "document", "", Value::Null);
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
            out["names"] = vault.request("secret.list", json!({"prefix": prefix}))?["names"].clone();
        }
        Ok(out)
    });

    let paths = h.clone();
    ext.command("demo.allowed", move |job: &Job| {
        Ok(json!({"path": paths.allowed(job.params["path"].as_str().unwrap_or_default())?}))
    });

    // the node's own requests, beyond commands
    let asked = h.clone();
    ext.method("demo.ask", move |params: Value| {
        if params["fail"] == true {
            bail!("asked to fail");
        }
        if let Some(ms) = params["sleep_ms"].as_u64() {
            thread::sleep(Duration::from_millis(ms));
        }
        Ok(json!({"params": params, "settings": asked.settings(), "thread": thread_name()}))
    });
    let relay = h.clone();
    ext.method("demo.relay", move |params: Value| {
        relay.request(params["method"].as_str().unwrap_or_default(), params["params"].clone())
    });

    ext.on_start(|h: &Handle| h.event("demo.started", json!({"node": h.node(), "settings": h.settings()})));
    ext.on_settings(|h: &Handle| -> anyhow::Result<()> {
        if h.settings()["label"] == "bad" {
            bail!("bad label");
        }
        h.event("demo.settings", h.settings());
        Ok(())
    });
    ext.every(
        Duration::from_millis(50),
        move |h: &Handle| h.event("demo.tick", json!({"ticks": ticks.fetch_add(1, Ordering::SeqCst) + 1})),
        "main",
    );

    ext.run()
}
