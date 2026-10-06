//! The hello example against the stand-in node, as the template's own tests run the Python one.

mod common;

use std::path::Path;

use common::{example, package, source};
use pointman_extension::testing::StandIn;
use serde_json::json;

fn hello() -> StandIn {
    StandIn::new(source("hello")).binary(example("hello"))
}

#[test]
fn greet_writes_the_greeting() {
    let node = hello().settings(json!({"greeting": "Hi"})).start().unwrap();
    let done = node.run("hello.greet", json!({"name": "James"})).unwrap();
    assert_eq!(done.result, json!({"text": "Hi, James!"}));
    assert_eq!(std::fs::read_to_string(&done.outputs[0].path).unwrap(), "Hi, James!\n");
    assert_eq!(done.outputs[0].kind, "text");
    assert_eq!(done.progress[0].fraction, Some(0.5));
    assert_eq!(node.events()["hello.last"], json!({"name": "James"}));
}

#[test]
fn the_default_greeting() {
    let node = hello().start().unwrap();
    assert_eq!(node.run("hello.greet", json!({"name": "Ada"})).unwrap().result, json!({"text": "Hello, Ada!"}));
}

#[test]
fn a_missing_name_fails() {
    let node = hello().start().unwrap();
    let failed = node.run("hello.greet", json!({})).unwrap_err();
    assert_eq!(failed.message, "name is missing");
    assert!(!failed.retry && !failed.cancelled);
}

#[test]
fn as_installed_it_runs_bin_hello_from_its_folder() {
    let root = package("hello");
    let node = StandIn::new(&root).start().unwrap();
    assert_eq!(node.info(), json!({"protocol": 1, "id": "hello", "version": "0.1.0"}));
    let done = node.run("hello.greet", json!({"name": "Grace"})).unwrap();
    assert_eq!(done.result, json!({"text": "Hello, Grace!"}));
    assert!(done.outputs[0].path.starts_with(node.work_dir()));
    node.stop();
    assert!(!node.running());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_binary_that_isnt_there_says_how_to_point_at_one() {
    let failed = StandIn::new(source("hello")).start().err().unwrap();
    assert!(failed.message.contains("can't start"), "{failed}");
    assert!(failed.message.contains("CARGO_BIN_EXE_"), "{failed}");
    assert!(Path::new(&source("hello")).join("extension.toml").exists());
}
