mod common;

use assert_cmd::Command;

// Every top-level JSON object carries api_version; `inspect --json`
// used to emit a bare process object without it.
#[test]
fn single_process_json_carries_api_version() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-api-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = format!("pmux-api-test-{}", std::process::id());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX");
        cmd
    };

    pmux()
        .args(["run", "-d", "-s", "probe", "sleep", "60"])
        .assert()
        .success();
    let outputs: Vec<serde_json::Value> = [&["inspect", "probe", "--json"][..]]
        .iter()
        .map(|args| {
            let out = pmux().args(*args).output().unwrap();
            serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null)
        })
        .collect();

    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .args(["-L", &socket, "kill-server"])
        .status();
    for json in outputs {
        assert_eq!(json["api_version"], 1, "{json}");
        assert_eq!(json["name"], "probe", "{json}");
        assert_eq!(json["lifecycle"], "running", "{json}");
    }
}

/// Field names and JSON types, ignoring values: `{"a": 1}` -> `{"a": "number"}`.
/// `null` matches any type, since optional fields are null on a plain process.
fn shape(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
        }
        Value::Array(items) => Value::Array(items.iter().take(1).map(shape).collect()),
        Value::Null => Value::Null,
        Value::Bool(_) => "bool".into(),
        Value::Number(_) => "number".into(),
        Value::String(_) => "string".into(),
    }
}

fn assert_same_shape(actual: &serde_json::Value, expected: &serde_json::Value, path: &str) {
    use serde_json::Value;
    match (actual, expected) {
        (Value::Null, _) | (_, Value::Null) => {}
        (Value::Object(a), Value::Object(e)) => {
            let mut a_keys: Vec<_> = a.keys().collect();
            let mut e_keys: Vec<_> = e.keys().collect();
            a_keys.sort();
            e_keys.sort();
            assert_eq!(a_keys, e_keys, "field names differ at {path}");
            for key in e.keys() {
                assert_same_shape(&a[key], &e[key], &format!("{path}.{key}"));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            if let (Some(a), Some(e)) = (a.first(), e.first()) {
                assert_same_shape(a, e, &format!("{path}[0]"));
            }
        }
        (a, e) => assert_eq!(a, e, "type differs at {path}"),
    }
}

// The live commands must emit exactly the documented v1 fields: the process
// object of tests/fixtures/api/state-v1.json, wrapped as each command wraps it.
#[test]
fn live_json_commands_match_the_published_v1_shape() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/api/state-v1.json")).unwrap();
    let state_shape = shape(&fixture);
    let process_shape = state_shape["sessions"][0].clone();

    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-shape-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = format!("pmux-shape-test-{}", std::process::id());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX");
        cmd
    };
    let json = |args: &[&str]| -> serde_json::Value {
        let out = pmux().args(args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let first = text.lines().next().unwrap_or("");
        // `events` prints JSON Lines; the others print one document.
        serde_json::from_str(first)
            .or_else(|_| serde_json::from_str(&text))
            .unwrap_or(serde_json::Value::Null)
    };

    pmux()
        .args(["run", "-d", "-s", "probe", "sleep", "60"])
        .assert()
        .success();
    let state = json(&["ls", "--json"]);
    let inspect = json(&["inspect", "probe", "--json"]);
    let event = {
        use std::io::BufRead;
        let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin!("pmux"))
            .args(["events"])
            .env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut first = String::new();
        std::io::BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut first)
            .unwrap();
        let _ = child.kill();
        let _ = child.wait();
        serde_json::from_str(&first).unwrap_or(serde_json::Value::Null)
    };
    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .args(["-L", &socket, "kill-server"])
        .status();

    assert!(!state.is_null() && !inspect.is_null() && !event.is_null());
    assert_same_shape(&shape(&state), &state_shape, "state");

    let mut inspect_shape = process_shape.clone();
    inspect_shape["api_version"] = "number".into();
    assert_same_shape(&shape(&inspect), &inspect_shape, "inspect");

    let event_shape = serde_json::json!({
        "api_version": "number",
        "event": "string",
        "emitted_at": "number",
        "state": state_shape,
    });
    assert_same_shape(&shape(&event), &event_shape, "events");
    assert_eq!(event["event"], "state");
}
