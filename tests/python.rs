//! Parity with Pointman core's Python node: the examples under its ExtensionHost and stand-in
//! (tests/python/host_check.py), and core's Python template under this crate's StandIn. Needs a
//! checkout of core: `POINTMAN_CORE=<its folder>`, and `POINTMAN_CORE_PYTHON` (a Python with core's
//! dependencies) if that isn't `<core>/.venv/bin/python`. Skipped without it, as on CI.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{package, tmp, ROOT};
use pointman_extension::testing::StandIn;
use serde_json::json;

struct Core {
    python: PathBuf,
    src: PathBuf,
    sdk: PathBuf,
    template: PathBuf,
}

fn core() -> Option<Core> {
    let Some(root) = std::env::var_os("POINTMAN_CORE").map(PathBuf::from) else {
        eprintln!("skipped: set POINTMAN_CORE to a checkout of Pointman's core to run this");
        return None;
    };
    let python =
        std::env::var_os("POINTMAN_CORE_PYTHON").map(PathBuf::from).unwrap_or_else(|| root.join(".venv/bin/python"));
    Some(Core {
        python,
        sdk: root.join("src/messageboard/extensions/sdk"),
        src: root.join("src"),
        template: root.join("sdk/template"),
    })
}

fn path_list(paths: &[&Path]) -> std::ffi::OsString {
    std::env::join_paths(paths).unwrap()
}

#[test]
fn the_examples_under_cores_python_node() {
    let Some(core) = core() else { return };
    let (hello, demo, state) = (package("hello"), package("demo"), tmp("python-node"));
    let out = Command::new(&core.python)
        .arg(Path::new(ROOT).join("tests/python/host_check.py"))
        .args([&hello, &demo, &state])
        .env("PYTHONPATH", path_list(&[&core.src, &core.sdk]))
        .output()
        .unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    println!("{stdout}");
    assert!(out.status.success(), "host_check.py failed:\n{stdout}\n{stderr}");
    for dir in [hello, demo, state] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[test]
fn cores_python_template_under_this_stand_in() {
    let Some(core) = core() else { return };
    let node = |settings| {
        StandIn::new(&core.template)
            .binary(&core.python) // its [run] is `python -m hello`
            .env("PYTHONPATH", &core.sdk)
            .settings(settings)
            .start()
            .unwrap()
    };
    let hi = node(json!({"greeting": "Hi"}));
    let done = hi.run("hello.greet", json!({"name": "James"})).unwrap();
    assert_eq!(done.result, json!({"text": "Hi, James!"}));
    assert_eq!(std::fs::read_to_string(&done.outputs[0].path).unwrap(), "Hi, James!\n");
    assert_eq!(done.progress[0].fraction, Some(0.5));
    assert_eq!(hi.events()["hello.last"], json!({"name": "James"}));
    let plain = node(json!({}));
    assert_eq!(plain.run("hello.greet", json!({"name": "Ada"})).unwrap().result, json!({"text": "Hello, Ada!"}));
    let failed = plain.run("hello.greet", json!({})).unwrap_err();
    assert_eq!((failed.message.as_str(), failed.retry), ("'name'", false));
}
