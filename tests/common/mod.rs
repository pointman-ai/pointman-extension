//! What the tests share: the examples cargo built, and folders laid out like an installed package.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// An example's source folder (its extension.toml).
pub fn source(name: &str) -> PathBuf {
    Path::new(ROOT).join("examples").join(name)
}

/// The example's binary, wherever this cargo puts it: `cargo build --example <name> --message-format=json` builds it
/// (or finds it built) and says where, as the `executable` of its compiler-artifact line. Where test binaries live
/// isn't the layout's to say (Rust 1.100 moved them under `target/<profile>/build/<pkg>/<hash>/`), so nothing is
/// worked out from the test's own path. Built once a run, with the test's profile and features.
pub fn example(name: &str) -> PathBuf {
    static BUILT: Mutex<Vec<(String, PathBuf)>> = Mutex::new(Vec::new());
    let mut built = BUILT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, bin)) = built.iter().find(|(n, _)| n == name) {
        return bin.clone();
    }
    let mut cargo = Command::new(env!("CARGO"));
    cargo.args(["build", "--example", name, "--message-format=json"]).current_dir(ROOT);
    if !cfg!(debug_assertions) {
        cargo.arg("--release");
    }
    if cfg!(feature = "proto") {
        cargo.args(["--features", "proto"]);
    }
    let out = cargo.output().unwrap();
    assert!(out.status.success(), "cargo build --example {name}: {}", String::from_utf8_lossy(&out.stderr));
    let bin = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|m| m["reason"] == "compiler-artifact" && m["target"]["name"] == name)
        .filter(|m| m["target"]["kind"].as_array().is_some_and(|k| k.iter().any(|k| k == "example")))
        .find_map(|m| m["executable"].as_str().map(PathBuf::from))
        .unwrap_or_else(|| panic!("cargo build --example {name} named no executable"));
    built.push((name.to_string(), bin.clone()));
    bin
}

/// A new empty folder for one test.
pub fn tmp(label: &str) -> PathBuf {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pointman-extension-{label}-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The example as a node installs it: extension.toml, and the binary at bin/<name>.
pub fn package(name: &str) -> PathBuf {
    let dir = tmp(&format!("package-{name}"));
    std::fs::copy(source(name).join("extension.toml"), dir.join("extension.toml")).unwrap();
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    std::fs::copy(example(name), dir.join("bin").join(name)).unwrap();
    dir
}
