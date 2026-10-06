//! Every part of the SDK, through the demo example under the stand-in node.

mod common;

use std::thread;
use std::time::{Duration, Instant};

use common::{example, source, tmp};
use pointman_extension::testing::{RunOptions, StandIn};
use serde_json::{json, Value};

const SECS: Duration = Duration::from_secs(5);

fn demo() -> StandIn {
    StandIn::new(source("demo")).binary(example("demo"))
}

fn span(result: &Value) -> (u64, u64) {
    (result["started"].as_u64().unwrap(), result["ended"].as_u64().unwrap())
}

#[test]
fn a_command_reports_progress_checkpoints_outputs_and_events() {
    let node = demo().start().unwrap();
    let done = node
        .run_with("demo.echo", json!({"a": 1}), RunOptions { cid: Some("e1".into()), ..Default::default() })
        .unwrap();
    assert_eq!(done.result["params"], json!({"a": 1}));
    assert_eq!(done.result["thread"], "cmd-e1"); // no lane: a thread of its own
    assert_eq!(done.result["checkpoint"], Value::Null);
    assert_eq!(done.result["work"], json!(node.work_dir().join("e1")));
    assert!(node.work_dir().join("e1").is_dir());

    assert_eq!(done.progress.len(), 2);
    assert_eq!((done.progress[0].fraction, done.progress[0].stage.as_deref()), (Some(0.5), Some("echoing")));
    assert_eq!(done.progress[0].detail, json!({}));
    assert_eq!((done.progress[1].fraction, done.progress[1].stage.as_deref()), (Some(1.0), None));
    assert_eq!(done.progress[1].detail, json!({"items": 1}));
    assert_eq!(done.checkpoints, vec![json!({"echoed": true})]);

    let out = &done.outputs[0];
    assert_eq!((out.kind.as_str(), out.role.as_str(), &out.meta), ("document", "main", &json!({})));
    assert_eq!(std::fs::read_to_string(&out.path).unwrap(), r#"{"a":1}"#);
    assert_eq!(node.events()["demo.echo"], json!({"last": {"a": 1}}));
}

#[test]
fn it_starts_with_the_node_and_the_settings_defaults() {
    let roots = tmp("roots");
    let node =
        demo().settings(json!({"limit": 5})).node("test-node").machine("test-machine").roots([&roots]).start().unwrap();
    assert_eq!(node.info(), json!({"protocol": 1, "id": "demo", "version": "0.2.0"}));
    let started = node.wait_event("demo.started", SECS).unwrap();
    assert_eq!(started["settings"], json!({"label": "demo", "limit": 5}));
    let n = &started["node"];
    assert_eq!(
        (n["protocol"].clone(), n["node"].clone(), n["machine"].clone()),
        (json!(1), json!("test-node"), json!("test-machine"))
    );
    assert_eq!(n["work_dir"], json!(node.work_dir()));
    assert_eq!(n["state_dir"], json!(node.state_dir_path()));
    assert_eq!(n["roots"].as_array().unwrap().len(), 1);
}

#[test]
fn settings_changes_reach_it() {
    let node = demo().start().unwrap();
    node.change_settings(json!({"label": "new"})).unwrap();
    assert_eq!(node.events()["demo.settings"], json!({"label": "new", "limit": 3}));
    assert_eq!(node.call("demo.ask", json!({})).unwrap()["settings"], json!({"label": "new", "limit": 3}));
    let refused = node.change_settings(json!({"label": "bad"})).unwrap_err();
    assert_eq!(refused.message, "bad label");
}

#[test]
fn a_lanes_commands_run_one_at_a_time_with_its_periodic_task_between_them() {
    let node = demo().start().unwrap();
    node.wait_event_where("demo.tick", SECS, |t| t["ticks"].as_u64() >= Some(2)).unwrap();
    let params = json!({"steps": 4, "step_ms": 60});
    let (a, b) = thread::scope(|s| {
        let a = s.spawn(|| node.run("demo.step", params.clone()).unwrap());
        let b = s.spawn(|| node.run("demo.step", params.clone()).unwrap());
        (a.join().unwrap(), b.join().unwrap())
    });
    for done in [&a, &b] {
        assert_eq!(done.result["thread"], "lane-main");
        assert_eq!(done.result["ticks_during"], 0, "the periodic task ran during a command");
        assert_eq!(done.checkpoints, (1..=4).map(|i| json!({"step": i})).collect::<Vec<_>>());
    }
    let ((a0, a1), (b0, b1)) = (span(&a.result), span(&b.result));
    assert!(a1 <= b0 || b1 <= a0, "they overlapped: {a0}-{a1} and {b0}-{b1}");
    let before = node.events()["demo.tick"]["ticks"].as_u64().unwrap();
    node.wait_event_where("demo.tick", SECS, |t| t["ticks"].as_u64() > Some(before)).unwrap();
}

#[test]
fn commands_with_no_lane_run_side_by_side() {
    let node = demo().start().unwrap();
    let (a, b) = thread::scope(|s| {
        let a = s.spawn(|| node.run("demo.echo", json!({"sleep_ms": 400})).unwrap());
        let b = s.spawn(|| node.run("demo.echo", json!({"sleep_ms": 400})).unwrap());
        (a.join().unwrap(), b.join().unwrap())
    });
    let ((a0, a1), (b0, b1)) = (span(&a.result), span(&b.result));
    assert!(a0 < b1 && b0 < a1, "they didn't overlap: {a0}-{a1} and {b0}-{b1}");
}

#[test]
fn cancelling_stops_a_command_at_its_next_check() {
    let node = demo().start().unwrap();
    let failed = thread::scope(|s| {
        let run = s.spawn(|| {
            let opts = RunOptions { cid: Some("long".into()), ..Default::default() };
            node.run_with("demo.step", json!({"steps": 200, "step_ms": 20}), opts)
        });
        let deadline = Instant::now() + SECS;
        while node.cancel("long").unwrap() != json!({"found": true}) {
            assert!(Instant::now() < deadline, "it never had the command");
            thread::sleep(Duration::from_millis(20));
        }
        run.join().unwrap().unwrap_err()
    });
    assert!(failed.cancelled && !failed.retry);
    assert_eq!(failed.message, "cancelled");
    assert_eq!(node.cancel("long").unwrap(), json!({"found": false}));
}

#[test]
fn it_resumes_from_a_checkpoint() {
    let node = demo().start().unwrap();
    let opts = RunOptions { checkpoint: Some(json!({"step": 3})), ..Default::default() };
    let done = node.run_with("demo.step", json!({"steps": 5}), opts).unwrap();
    assert_eq!((done.result["from"].clone(), done.result["to"].clone()), (json!(3), json!(5)));
    assert_eq!(done.checkpoints, vec![json!({"step": 4}), json!({"step": 5})]);
}

#[test]
fn failures_say_whether_to_try_again() {
    let node = demo().start().unwrap();
    let fail = |params: Value| node.run("demo.fail", params).unwrap_err();

    let retry = fail(json!({"how": "retry", "message": "the app isn't open"}));
    assert_eq!((retry.message.as_str(), retry.retry, retry.cancelled), ("the app isn't open", true, false));
    assert_eq!(fail(json!({"how": "retry", "message": ""})).message, "try again later");
    let wrapped = fail(json!({"how": "wrapped", "message": "busy"}));
    assert_eq!((wrapped.message.as_str(), wrapped.retry), ("while asking: busy", true));

    let plain = fail(json!({"message": "token was refused"}));
    assert_eq!((plain.message.as_str(), plain.retry), ("token was refused", false));
    let panicked = fail(json!({"how": "panic", "message": "boom"}));
    assert_eq!((panicked.message.as_str(), panicked.retry), ("panicked: boom", false));
    let event = fail(json!({"how": "event"}));
    assert_eq!(event.message, "panicked: event other.thing must start with demo.");
    assert!(node.log().contains("demo.fail"), "a failure is logged: {}", node.log());

    let unknown = node.run("demo.nope", json!({})).unwrap_err();
    assert_eq!(unknown.message, "demo.nope isn't a command of demo");
    // and it carries on
    assert_eq!(node.run("demo.echo", json!({"after": true})).unwrap().result["params"], json!({"after": true}));
}

#[test]
fn secrets_only_within_its_permissions() {
    let node = demo()
        .secrets([("demo.work", "w"), ("demo.spare", "s"), ("shared.key", "k"), ("other.key", "o"), ("demo", "d")])
        .start()
        .unwrap();
    let secret = |params: Value| node.run("demo.secret", params);
    assert_eq!(secret(json!({"name": "demo.work"})).unwrap().result, json!({"value": "w"}));
    assert_eq!(secret(json!({"name": "shared.key"})).unwrap().result, json!({"value": "k"}));
    let refused = secret(json!({"name": "other.key"})).unwrap_err();
    assert_eq!(refused.message, "demo may not use the secret 'other.key': it isn't in its permissions");
    assert_eq!(secret(json!({"name": "demo.gone"})).unwrap_err().message, "no secret 'demo.gone' in the vault");
    assert_eq!(secret(json!({"prefix": "demo."})).unwrap().result, json!({"names": ["demo.spare", "demo.work"]}));
    assert_eq!(
        secret(json!({"prefix": ""})).unwrap().result,
        json!({"names": ["demo.spare", "demo.work", "shared.key"]})
    );
    let asked = node.asked();
    assert_eq!(asked[0], ("secret.get".to_string(), json!({"name": "demo.work"})));
    assert_eq!(asked.last().unwrap(), &("secret.list".to_string(), json!({"prefix": ""})));
}

#[test]
fn the_nodes_own_requests_run_side_by_side() {
    let node = demo().start().unwrap();
    let answer = node.call("demo.ask", json!({"q": 1})).unwrap();
    assert_eq!(answer["params"], json!({"q": 1}));
    assert_eq!(answer["thread"], "method-demo.ask");
    assert_eq!(node.call("demo.ask", json!({"fail": true})).unwrap_err().message, "asked to fail");
    assert_eq!(node.call("demo.nope", json!({})).unwrap_err().message, "no method demo.nope");

    thread::scope(|s| {
        let slow = s.spawn(|| node.call("demo.ask", json!({"sleep_ms": 1500})).unwrap());
        thread::sleep(Duration::from_millis(50));
        node.call("demo.ask", json!({})).unwrap();
        assert!(!slow.is_finished(), "a slow request held up the next");
        slow.join().unwrap();
    });
    let timed_out = node.call_timeout("demo.ask", json!({"sleep_ms": 1500}), Duration::from_millis(200)).unwrap_err();
    assert!(timed_out.message.starts_with("demo didn't answer demo.ask"), "{timed_out}");
}

#[test]
fn it_asks_the_node_and_gets_its_answers_and_reasons() {
    let node = demo()
        .method("demo.lookup", |params: Value| Ok(json!({"found": params})))
        .method("demo.refuse", |_| -> anyhow::Result<Value> { anyhow::bail!("not today") })
        .start()
        .unwrap();
    let relay = |method: &str| node.call("demo.relay", json!({"method": method, "params": {"x": 1}}));
    assert_eq!(relay("demo.lookup").unwrap(), json!({"found": {"x": 1}}));
    assert_eq!(relay("demo.refuse").unwrap_err().message, "not today");
    assert_eq!(relay("node.unknown").unwrap_err().message, "no method node.unknown");
    assert_eq!(node.asked().len(), 3);
}

#[test]
fn paths_only_inside_the_nodes_folders() {
    let roots = tmp("allowed");
    std::fs::create_dir_all(roots.join("project")).unwrap();
    let node = demo().roots([roots.join("project")]).start().unwrap();
    let allowed = |path: &std::path::Path| node.run("demo.allowed", json!({"path": path}));
    let inside = allowed(&roots.join("project/shot.blend")).unwrap().result;
    assert!(inside["path"].as_str().unwrap().ends_with("project/shot.blend"));
    assert!(allowed(&node.work_dir().join("x/y.png")).is_ok());
    let outside = allowed(&roots.join("project/../elsewhere")).unwrap_err();
    assert!(outside.message.ends_with("is outside the folders this node lets extensions use"), "{outside}");
    assert!(!outside.retry);
}

#[test]
fn a_crash_fails_whats_running_with_retry() {
    let node = demo().start().unwrap();
    let failed = node.run("demo.fail", json!({"how": "exit"})).unwrap_err();
    assert!(failed.retry, "{failed:?}");
    assert!(failed.message.starts_with("demo stopped"), "{failed}");
    assert!(!node.running());
    let after = node.call("demo.ask", json!({})).unwrap_err();
    assert!(after.message.starts_with("demo isn't running"), "{after}");
}

#[test]
fn it_stops_when_asked_and_cleans_up() {
    let node = demo().start().unwrap();
    let state = node.state_dir_path().to_path_buf();
    assert!(state.exists() && node.running());
    node.stop();
    assert!(!node.running());
    assert!(!state.exists(), "the temporary state folder is removed");
    node.stop(); // twice is fine
}

#[test]
fn a_given_state_dir_is_kept() {
    let dir = tmp("state");
    let node = demo().state_dir(&dir).env("DEMO_EXTRA", "1").start().unwrap();
    assert_eq!(node.state_dir_path(), dir.join("extensions/demo"));
    node.run("demo.echo", json!({})).unwrap();
    drop(node);
    assert!(dir.join("extensions/demo/work").is_dir());
    std::fs::remove_dir_all(dir).unwrap();
}
