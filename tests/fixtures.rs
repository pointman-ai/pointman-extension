//! The protocol's fixtures (Pointman's proto/fixtures/extension/<method>.json, one per method, and
//! proto/fixtures/panes/edits.json), copied here: core's tests, the node's and both SDKs' read the same files.

use std::path::Path;

use pointman_extension::edits;
use pointman_extension::v1::{PaneContent, PaneEdit};
use pointman_extension::{Side, METHODS};
use serde_json::Value;

fn fixtures() -> Vec<(String, Value)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension");
    let mut out: Vec<(String, Value)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap()))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn every_method_has_a_fixture_and_every_fixture_a_method() {
    let files: Vec<String> = fixtures().into_iter().map(|(name, _)| name).collect();
    let mut methods: Vec<String> = METHODS.iter().map(|m| m.method.to_string()).collect();
    methods.sort();
    assert_eq!(files, methods);
    assert_eq!(METHODS.len(), 58, "20 + 8 requests and notifications to the extension, 16 + 14 to the node");
    let to_extension = METHODS.iter().filter(|m| m.side == Side::Process).count();
    assert_eq!(to_extension, 28);
}

#[test]
fn each_fixture_reads_as_its_message_and_writes_back_the_same() {
    for (name, fixture) in fixtures() {
        let info = METHODS.iter().find(|m| m.method == name).unwrap_or_else(|| panic!("no method {name}"));
        assert_eq!(fixture["method"], name.as_str(), "{name}'s own method");
        let params = &fixture["params"];
        assert_eq!((info.round_trip.0)(params).unwrap_or_else(|e| panic!("{name} params: {e}")), *params, "{name} params");
        match (info.notification, fixture.get("result")) {
            (true, None) => {}
            (false, Some(result)) => {
                assert_eq!((info.round_trip.1)(result).unwrap_or_else(|e| panic!("{name} result: {e}")), *result, "{name} result")
            }
            (notification, result) => panic!("{name}: notification {notification}, result {result:?}"),
        }
    }
}

#[test]
fn a_newer_node_is_read_by_skipping_what_it_doesnt_know() {
    let info = METHODS.iter().find(|m| m.method == "command.start").unwrap();
    let mut params = fixtures().into_iter().find(|(n, _)| n == "command.start").unwrap().1["params"].clone();
    let want = (info.round_trip.0)(&params).unwrap();
    params["fromANewerNode"] = serde_json::json!({"anything": [1, 2]});
    assert_eq!((info.round_trip.0)(&params).unwrap(), want, "an unknown field is skipped");
    let info = METHODS.iter().find(|m| m.method == "agents.join").unwrap();
    let mut params = fixtures().into_iter().find(|(n, _)| n == "agents.join").unwrap().1["params"].clone();
    params["effort"] = "EFFORT_ULTRA".into();
    let got = (info.round_trip.0)(&params);
    assert!(got.is_ok(), "an unknown enum value is skipped, not refused: {got:?}");
}

#[test]
fn every_edit_case_applies_as_written_or_is_refused_whole() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/panes/edits.json");
    let cases: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let cases = cases["cases"].as_array().unwrap();
    assert!(cases.len() >= 10);
    for case in cases {
        let title = case["title"].as_str().unwrap();
        let content: PaneContent = serde_json::from_value(case["content"].clone()).unwrap();
        let list: Vec<PaneEdit> = serde_json::from_value(case["edits"].clone()).unwrap();
        let got = edits::apply(&content, &list);
        if case.get("refused").is_some() {
            assert!(got.is_err(), "{title}: should be refused");
            continue;
        }
        let got = got.unwrap_or_else(|e| panic!("{title}: {e}"));
        assert_eq!(serde_json::to_value(&got).unwrap(), case["after"], "{title}");
        assert_eq!(serde_json::to_value(&content).unwrap(), case["content"], "{title}: what it had stays as it was");
    }
}
