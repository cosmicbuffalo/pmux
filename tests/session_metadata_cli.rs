//! Per-session metadata must follow the live tmux session, not its name.
//! Each test runs pmux against a private tmux socket and HOME.

mod common;

use assert_cmd::Command;
use std::path::{Path, PathBuf};

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
                .prefix("pmux-md-")
                .tempdir_in("/tmp")
                .unwrap(),
            socket: format!("pmux-{tag}-{}", std::process::id()),
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

    fn tmux(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new("tmux");
        cmd.env("TMUX_TMPDIR", self.tmux_tmp.path())
            .env_remove("TMUX")
            .args(["-L", &self.socket, "-f", "/dev/null"]);
        cmd
    }

    // State for a non-default socket lives under sockets/<hex>.
    fn state_dir(&self) -> PathBuf {
        let encoded: String = self.socket.bytes().map(|b| format!("{b:02x}")).collect();
        self.home
            .path()
            .join(".local/state/pmux/sockets")
            .join(encoded)
    }

    fn plant(&self, dir: &str, name: &str, contents: &str) -> PathBuf {
        let dir = self.state_dir().join(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

// Regression: agent identity and host-pane files survive a crash or reboot
// (restore needs them), and a new session that reused the name inherited
// the old session's agent identity and host pane.
#[test]
fn fresh_session_does_not_inherit_a_dead_sessions_metadata() {
    let Some(sandbox) = Sandbox::new("reuse") else {
        return;
    };
    let identity = sandbox.plant(
        "agent_sessions",
        "reuse",
        "claude\t11111111-2222-3333-4444-555555555555\t0\n",
    );
    let host = sandbox.plant("host_panes", "reuse", "%42\t/tmp/outer.sock\n");

    sandbox
        .pmux()
        .args(["run", "-d", "-s", "reuse", "sleep", "60"])
        .assert()
        .success();

    assert_eq!(read(&identity), None, "stale agent identity survived");
    assert_eq!(read(&host), None, "stale host pane survived");
    let launch = sandbox.state_dir().join("launch_cmds/reuse");
    assert_eq!(read(&launch).as_deref(), Some("sleep 60\n"));
}

// Regression: renaming onto the name of a dead session failed because its
// leftover metadata files counted as a conflict.
#[test]
fn rename_onto_a_dead_sessions_name_replaces_its_stale_metadata() {
    let Some(sandbox) = Sandbox::new("rename") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "old", "sleep", "60"])
        .assert()
        .success();
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "busy", "sleep", "60"])
        .assert()
        .success();
    let stale_cmd = sandbox.plant("launch_cmds", "gone", "codex\n");
    let stale_identity = sandbox.plant("agent_sessions", "gone", "codex\tabc\t1\n");

    sandbox
        .pmux()
        .args(["rename-session", "old", "gone"])
        .assert()
        .success();
    assert_eq!(read(&stale_cmd).as_deref(), Some("sleep 60\n"));
    assert_eq!(read(&stale_identity), None);
    assert_eq!(read(&sandbox.state_dir().join("launch_cmds/old")), None);

    // A live name still refuses, and keeps both sessions' metadata.
    sandbox
        .pmux()
        .args(["rename-session", "gone", "busy"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already exists"));
    assert_eq!(read(&stale_cmd).as_deref(), Some("sleep 60\n"));
    let busy = sandbox.state_dir().join("launch_cmds/busy");
    assert_eq!(read(&busy).as_deref(), Some("sleep 60\n"));
}

// Regression: metadata cleanup listed sessions without the state lock and
// then deleted files for every name missing from that (possibly stale)
// list, wiping metadata that a concurrent `pmux run -d` had just written.
#[test]
fn concurrent_spawns_and_snapshots_keep_every_sessions_metadata() {
    let Some(sandbox) = Sandbox::new("race") else {
        return;
    };
    // Start the server so later spawns and snapshots overlap.
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "base", "sleep", "60"])
        .assert()
        .success();
    let mut names: Vec<String> = (0..6).map(|i| format!("race{i}")).collect();
    std::thread::scope(|scope| {
        for name in &names {
            let sandbox = &sandbox;
            scope.spawn(move || {
                sandbox
                    .pmux()
                    .args(["run", "-d", "-s", name, "sleep", "60"])
                    .assert()
                    .success();
            });
            scope.spawn(move || {
                let _ = sandbox.pmux().arg("snapshot").output();
            });
        }
    });
    names.push("base".to_string());
    for name in &names {
        let launch = sandbox.state_dir().join("launch_cmds").join(name);
        assert_eq!(
            read(&launch).as_deref(),
            Some("sleep 60\n"),
            "metadata for {name} was lost"
        );
    }
}

// History discovery deliberately tolerates unavailable stores. A successful
// but incomplete scan must not erase labels on unrelated live processes.
#[test]
fn status_title_survives_missing_history_and_attach_retries_discovery() {
    let Some(sandbox) = Sandbox::new("labels") else {
        return;
    };
    let name = "1790869355-vurcl60h";
    let target = format!("={name}:");
    let claude = sandbox.home.path().join(".claude");
    let pmux = || {
        let mut cmd = sandbox.pmux();
        cmd.env("CLAUDE_CONFIG_DIR", &claude)
            .env("CODEX_HOME", sandbox.home.path().join(".codex"));
        cmd
    };
    pmux()
        .args(["run", "-d", "-s", name, "sleep", "60"])
        .assert()
        .success();
    let epoch = sandbox
        .tmux()
        .args(["display-message", "-p", "-t", &target, "#{session_created}"])
        .output()
        .unwrap();
    assert!(epoch.status.success());
    let epoch = String::from_utf8(epoch.stdout).unwrap();
    let id = "11111111-2222-3333-4444-555555555555";
    sandbox.plant("agent_sessions", name, &format!("claude\t{id}\t{epoch}"));
    let project = claude.join("projects/test");
    std::fs::create_dir_all(&project).unwrap();
    let transcript = project.join(format!("{id}.jsonl"));
    let write_title = |title| {
        std::fs::write(
            &transcript,
            serde_json::json!({"type": "custom-title", "customTitle": title}).to_string() + "\n",
        )
        .unwrap();
    };
    let label = || {
        let output = sandbox
            .tmux()
            .args(["show-options", "-qv", "-t", &target, "@pmux_status_label"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    };
    write_title("Original title");
    pmux().arg("status-labels").assert().success();
    assert_eq!(label(), "Original title");

    std::fs::remove_file(&transcript).unwrap();
    pmux().arg("status-labels").assert().success();
    assert_eq!(label(), "Original title", "missing history erased the pill");

    // The agent writes a title later without another pane-title event. The
    // same hooks tmux fires on attach/switch should recover it independently
    // of new-process creation.
    for hook in ["client-attached", "client-session-changed"] {
        write_title(hook);
        assert!(
            sandbox
                .tmux()
                .args(["set-hook", "-R", "-t", &target, hook])
                .status()
                .unwrap()
                .success()
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while label() != hook {
            assert!(
                std::time::Instant::now() < deadline,
                "{hook} did not refresh the title"
            );
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
    }
}
