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

/// The example's binary. `cargo test` builds the examples beside the tests; built here if a
/// filtered run (`cargo test --test hello`) didn't.
pub fn example(name: &str) -> PathBuf {
    static BUILD: Mutex<()> = Mutex::new(());
    let exe = std::env::current_exe().unwrap();
    let profile = exe.parent().and_then(Path::parent).unwrap();
    let bin = profile.join("examples").join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    let _once = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if !bin.exists() {
        let mut cargo = Command::new(env!("CARGO"));
        cargo.args(["build", "--example", name]).current_dir(ROOT);
        if profile.ends_with("release") {
            cargo.arg("--release");
        }
        assert!(cargo.status().unwrap().success(), "cargo build --example {name}");
    }
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
