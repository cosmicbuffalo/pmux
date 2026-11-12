//! Spawned-by lineage: a process started from inside a pmux pane records the
//! pane's process as its parent. Each test runs pmux against a private tmux
//! socket and HOME.

mod common;

use assert_cmd::Command;
use serde_json::Value;
use std::time::{Duration, Instant};

struct Sandbox {
    home: tempfile::TempDir,
    tmux_tmp: tempfile::TempDir,
    socket: String,
}

impl Sandbox {
    fn new(tag: &str) -> Option<Self> {
        if std::process::Command::new("tmux")
            .arg("-V")
            .output()
            .is_err()
        {
            return None;
        }
        Some(Self {
            home: tempfile::tempdir().unwrap(),
            tmux_tmp: tempfile::Builder::new()
                .prefix("pmux-ln-")
                .tempdir_in("/tmp")
                .unwrap(),
            socket: format!("pmux-lineage-{tag}-{}", std::process::id()),
        })
    }

    fn pmux(&self) -> Command {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", self.home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", self.tmux_tmp.path())
            .env("PMUX_SOCKET", &self.socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        cmd
    }

    /// pmux as run by a process in `pane`, with `$TMUX` naming `socket_path`.
    fn pmux_in_pane(&self, socket_path: &str, pane: &str) -> Command {
        let mut cmd = self.pmux();
        cmd.env("TMUX", format!("{socket_path},1,0"))
            .env("TMUX_PANE", pane);
        cmd
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = std::process::Command::new("tmux")
            .env("TMUX_TMPDIR", self.tmux_tmp.path())
            .env_remove("TMUX")
            .args(["-L", &self.socket, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn run(&self, name: &str) {
        self.pmux()
            .args(["run", "-d", "--no-parent", "-s", name, "sleep", "600"])
            .assert()
            .success();
    }

    fn pane_of(&self, name: &str) -> String {
        self.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={name}:"),
            "#{pane_id}",
        ])
    }

    fn socket_path(&self) -> String {
        self.tmux(&["display-message", "-p", "#{socket_path}"])
    }

    fn created(&self, name: &str) -> i64 {
        self.tmux(&[
            "display-message",
            "-p",
            "-t",
            &format!("={name}:"),
            "#{session_created}",
        ])
        .parse()
        .unwrap()
    }

    fn spawned_by(&self, name: &str) -> Value {
        let output = self
            .pmux()
            .args(["inspect", "--json", name])
            .output()
            .unwrap();
        assert!(output.status.success(), "inspect {name} failed");
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        json.get("spawned_by").cloned().unwrap_or(Value::Null)
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        self.tmux(&["kill-server"]);
    }
}

fn local(process: &str, created_at: i64) -> Value {
    serde_json::json!({
        "process": process,
        "created_at": created_at,
        "machine": null,
        "url": null,
    })
}

#[test]
fn a_process_started_from_a_pane_records_that_panes_process() {
    let Some(sandbox) = Sandbox::new("auto") else {
        return;
    };
    sandbox.run("parent");
    let pane = sandbox.pane_of("parent");
    let socket = sandbox.socket_path();

    sandbox
        .pmux_in_pane(&socket, &pane)
        .args(["run", "-d", "-s", "child", "sleep", "600"])
        .assert()
        .success();
    assert_eq!(
        sandbox.spawned_by("child"),
        local("parent", sandbox.created("parent"))
    );

    // --no-parent records none, from the same pane.
    sandbox
        .pmux_in_pane(&socket, &pane)
        .args(["run", "-d", "--no-parent", "-s", "loner", "sleep", "600"])
        .assert()
        .success();
    assert_eq!(sandbox.spawned_by("loner"), Value::Null);

    // The same pane id named through another socket path is not trusted.
    let elsewhere = format!("/nonexistent/{}", sandbox.socket);
    sandbox
        .pmux_in_pane(&elsewhere, &pane)
        .args(["run", "-d", "-s", "forged", "sleep", "600"])
        .assert()
        .success();
    assert_eq!(sandbox.spawned_by("forged"), Value::Null);

    // Nothing is recorded for a pane that does not exist, and spawning works.
    sandbox
        .pmux_in_pane(&socket, "%999")
        .args(["run", "-d", "-s", "nopane", "sleep", "600"])
        .assert()
        .success();
    assert_eq!(sandbox.spawned_by("nopane"), Value::Null);

    // The human view names the parent.
    sandbox
        .pmux()
        .args(["inspect", "child"])
        .assert()
        .success()
        .stdout(predicates::str::contains("spawned by parent\n"));
}

#[test]
fn a_real_nested_spawn_records_its_parent() {
    let Some(sandbox) = Sandbox::new("nested") else {
        return;
    };
    let bin = assert_cmd::cargo::cargo_bin!("pmux");
    let script = format!("'{}' run -d -s child sleep 600; sleep 600", bin.display());
    sandbox
        .pmux()
        .args([
            "run",
            "-d",
            "--no-parent",
            "-s",
            "parent",
            "sh",
            "-c",
            &script,
        ])
        .assert()
        .success();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sandbox
        .tmux(&["list-sessions", "-F", "#{session_name}"])
        .lines()
        .all(|name| name != "child")
    {
        assert!(Instant::now() < deadline, "child never started");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The inner `run -d` records the parent just after its session appears,
    // so wait for the record rather than reading it the moment the session
    // exists.
    let expected = local("parent", sandbox.created("parent"));
    let mut recorded = sandbox.spawned_by("child");
    while recorded != expected && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
        recorded = sandbox.spawned_by("child");
    }
    assert_eq!(recorded, expected);
}

#[test]
fn explicit_parents_local_and_remote() {
    let Some(sandbox) = Sandbox::new("explicit") else {
        return;
    };
    sandbox.run("boss");
    sandbox
        .pmux()
        .args([
            "run", "-d", "-s", "worker", "--parent", "boss", "sleep", "600",
        ])
        .assert()
        .success();
    assert_eq!(
        sandbox.spawned_by("worker"),
        local("boss", sandbox.created("boss"))
    );

    // A missing local parent fails the spawn and creates nothing.
    sandbox
        .pmux()
        .args([
            "run", "-d", "-s", "stray", "--parent", "nobody", "sleep", "600",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not running"));
    assert!(
        !sandbox
            .tmux(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .any(|name| name == "stray")
    );

    // Remote flags must come together.
    sandbox
        .pmux()
        .args([
            "run",
            "-d",
            "--parent",
            "x",
            "--parent-machine",
            "macbook",
            "sleep",
            "1",
        ])
        .assert()
        .failure();

    sandbox
        .pmux()
        .args([
            "run",
            "-d",
            "-s",
            "far",
            "--parent",
            "lineage ios",
            "--parent-machine",
            "macbook",
            "--parent-url",
            "http://100.72.15.45:9470",
            "sleep",
            "600",
        ])
        .assert()
        .success();
    assert_eq!(
        sandbox.spawned_by("far"),
        serde_json::json!({
            "process": "lineage ios",
            "created_at": null,
            "machine": "macbook",
            "url": "http://100.72.15.45:9470",
        })
    );
    sandbox
        .pmux()
        .args(["parent", "far"])
        .assert()
        .success()
        .stdout("spawned by lineage ios on macbook\n");
}

#[test]
fn renames_follow_both_parent_and_child() {
    let Some(sandbox) = Sandbox::new("rename") else {
        return;
    };
    sandbox.run("p");
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "c", "--parent", "p", "sleep", "600"])
        .assert()
        .success();
    let created = sandbox.created("p");

    sandbox
        .pmux()
        .args(["rename-session", "p", "p2"])
        .assert()
        .success();
    assert_eq!(sandbox.spawned_by("c"), local("p2", created));

    sandbox
        .pmux()
        .args(["rename-session", "c", "c2"])
        .assert()
        .success();
    assert_eq!(sandbox.spawned_by("c2"), local("p2", created));
}

#[test]
fn a_dead_parent_is_omitted_and_its_name_is_not_reused() {
    let Some(sandbox) = Sandbox::new("dead") else {
        return;
    };
    sandbox.run("p");
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "c", "--parent", "p", "sleep", "600"])
        .assert()
        .success();
    sandbox.pmux().args(["kill", "p"]).assert().success();
    assert_eq!(sandbox.spawned_by("c"), Value::Null);

    // A later process named "p" does not adopt the orphan. tmux epochs have
    // one-second resolution, so wait for a distinct one.
    std::thread::sleep(Duration::from_millis(1100));
    sandbox.run("p");
    assert_eq!(sandbox.spawned_by("c"), Value::Null);
    sandbox
        .pmux()
        .args(["parent", "c"])
        .assert()
        .success()
        .stdout(predicates::str::contains("has ended"));
}

#[test]
fn the_parent_command_sets_and_clears() {
    let Some(sandbox) = Sandbox::new("adopt") else {
        return;
    };
    sandbox.run("a");
    sandbox.run("b");
    sandbox
        .pmux()
        .args(["parent", "b"])
        .assert()
        .success()
        .stdout("no parent\n");
    sandbox
        .pmux()
        .args(["parent", "b", "--set", "a"])
        .assert()
        .success()
        .stdout("spawned by a\n");
    assert_eq!(sandbox.spawned_by("b"), local("a", sandbox.created("a")));

    // No cycles, and no self-parenting.
    sandbox
        .pmux()
        .args(["parent", "a", "--set", "b"])
        .assert()
        .failure();
    sandbox
        .pmux()
        .args(["parent", "a", "--set", "a"])
        .assert()
        .failure();

    sandbox
        .pmux()
        .args(["parent", "b", "--json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"process\": \"a\""));
    sandbox
        .pmux()
        .args(["parent", "b", "--clear"])
        .assert()
        .success()
        .stdout("no parent\n");
    assert_eq!(sandbox.spawned_by("b"), Value::Null);
}

#[test]
fn ls_and_kill_select_only_the_callers_own_spawned_processes() {
    let Some(sandbox) = Sandbox::new("children") else {
        return;
    };
    sandbox.run("parent");
    sandbox.run("stranger");
    let socket = sandbox.socket_path();
    let parent_pane = sandbox.pane_of("parent");
    for child in ["child-a", "child-b"] {
        sandbox
            .pmux_in_pane(&socket, &parent_pane)
            .args(["run", "-d", "-s", child, "sleep", "600"])
            .assert()
            .success();
    }
    let child_pane = sandbox.pane_of("child-a");
    sandbox
        .pmux_in_pane(&socket, &child_pane)
        .args(["run", "-d", "-s", "grandchild", "sleep", "600"])
        .assert()
        .success();

    let names = |args: &[&str]| -> Vec<String> {
        let output = sandbox
            .pmux_in_pane(&socket, &parent_pane)
            .args(["ls", "--json"])
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "ls {args:?} failed");
        let json: Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut names: Vec<String> = json["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|process| {
                assert!(process["spawned_by"].is_object(), "spawned_by kept");
                process["name"].as_str().unwrap().to_string()
            })
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&["--children"]), ["child-a", "child-b"]);
    assert_eq!(
        names(&["--descendants"]),
        ["child-a", "child-b", "grandchild"]
    );
    assert_eq!(names(&["--children", "--of", "child-a"]), ["grandchild"]);

    // Outside a pmux pane there is no caller to default to.
    sandbox
        .pmux()
        .args(["ls", "--children"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--of NAME"));
    sandbox
        .pmux()
        .args(["kill", "--children"])
        .assert()
        .failure();

    // kill --children leaves the grandchild, the caller and strangers alone.
    sandbox
        .pmux_in_pane(&socket, &parent_pane)
        .args(["kill", "--children"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Killed pmux process 'child-a'."))
        .stdout(predicates::str::contains("Killed pmux process 'child-b'."));
    let alive = sandbox.tmux(&["list-sessions", "-F", "#{session_name}"]);
    let mut alive: Vec<&str> = alive.lines().collect();
    alive.sort();
    assert_eq!(alive, ["grandchild", "parent", "stranger"]);

    sandbox
        .pmux_in_pane(&socket, &parent_pane)
        .args(["kill", "--children"])
        .assert()
        .success()
        .stdout("No child processes to kill.\n");
}
