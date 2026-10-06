//! The extension's process: the node starts it as extension.toml's `[run]` says (`bin/hello`, from
//! the extension's folder) and talks to it over stdin and stdout (the SDK handles that). Log with
//! `h.log(…)`: stdout is the node's.

use anyhow::Context;
use pointman_extension::{Extension, Job};
use serde_json::json;

fn main() -> anyhow::Result<()> {
    let mut ext = Extension::new()?;
    let h = ext.handle();

    ext.command("hello.greet", move |job: &Job| {
        let name = job.params["name"].as_str().context("name is missing")?;
        let greeting = h.settings()["greeting"].as_str().context("no greeting setting")?.to_string();
        let text = format!("{greeting}, {name}!");
        job.progress(0.5, "writing");
        let out = job.work.join("greeting.txt");
        std::fs::write(&out, format!("{text}\n"))?;
        job.output(&out, "text", "main", json!({})); // posted where the request came from
        h.event("hello.last", json!({"name": name}));
        Ok(json!({"text": text}))
    });

    ext.run()
}
