//! Fresh startup generations must not inherit pre-respawn completion signals.

mod common;

use assert_cmd::Command;
use std::path::PathBuf;

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
            .env("TMUX_TMPDIR", self.tmux_tmp.path())
            .env("PMUX_SOCKET", &self.socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("AI_ATTN_STATE_DIR", self.home.path().join("signals"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.home.path().join("bin").display(),
                    common::path()
                ),
            );
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
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

fn signal(sandbox: &Sandbox, state: &str) {
    let dir = sandbox.home.path().join("signals");
    std::fs::create_dir_all(&dir).unwrap();
    let value = serde_json::json!({"agent":"codex", "state":state, "reason":"PermissionRequest", "updated_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()});
    std::fs::write(dir.join("next"), value.to_string()).unwrap();
    std::fs::rename(dir.join("next"), dir.join("agent.json")).unwrap();
}

#[test]
fn respawned_startup_screens_reject_old_signals_and_return_not_ready() {
    let Some(sandbox) = Sandbox::new("startup") else {
        return;
    };
    let bin = sandbox.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("codex");
    std::fs::write(&fake, "#!/bin/sh\nprintf 'Update available. Press Enter to update, Escape to skip.\\n'\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "agent", "codex"])
        .assert()
        .success();
    signal(&sandbox, "done");
    sandbox
        .pmux()
        .args(["wait", "agent", "--until", "agent:done", "--timeout", "1s"])
        .assert()
        .success();
    sandbox
        .pmux()
        .args([
            "respawn",
            "agent",
            "--resume-id",
            "startup-id",
            "--force",
            "--yes",
        ])
        .assert()
        .success();
    let out = sandbox
        .pmux()
        .args(["inspect", "agent", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        json["agent"]["state"].is_null(),
        "old done signal leaked: {json}"
    );
    assert!(json["startup"]["started_at"].is_number());
    assert_eq!(json["startup"]["capture_command"], "pmux capture agent");
    sandbox
        .pmux()
        .args(["inspect", "agent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("readiness unconfirmed"))
        .stdout(predicates::str::contains("pmux capture agent"));
    sandbox
        .pmux()
        .args([
            "wait",
            "agent",
            "--until",
            "agent:done",
            "--startup-timeout",
            "0s",
            "--json",
        ])
        .timeout(std::time::Duration::from_secs(2))
        .assert()
        .code(3)
        .stdout(predicates::str::contains("not_ready"));
    sandbox
        .pmux()
        .args(["wait", "agent", "--timeout", "1ms", "--json"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("timed_out"));
    // A changed record in the same second is current, unlike the exact old record.
    signal(&sandbox, "waiting");
    sandbox
        .pmux()
        .args([
            "wait",
            "agent",
            "--until",
            "agent:waiting",
            "--timeout",
            "1s",
        ])
        .assert()
        .success();
    sandbox
        .pmux()
        .args(["wait", "agent", "--until", "agent:done", "--timeout", "1s"])
        .assert()
        .code(2);
    let mut signal_value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.home.path().join("signals/agent.json")).unwrap(),
    )
    .unwrap();
    signal_value["state"] = "done".into();
    signal_value["updated_at"] = (signal_value["updated_at"].as_i64().unwrap() + 1).into();
    std::fs::write(
        sandbox.home.path().join("signals/agent.json"),
        signal_value.to_string(),
    )
    .unwrap();
    sandbox
        .pmux()
        .args(["wait", "agent", "--until", "agent:done", "--timeout", "1s"])
        .assert()
        .success();
    // Make the signal unavailable, and prove state distinguishes an older startup.
    std::fs::remove_file(sandbox.home.path().join("signals/agent.json")).unwrap();
    let start_path = sandbox.state_dir().join("agent_starts/agent");
    let mut start: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&start_path).unwrap()).unwrap();
    start["started_at"] = (start["started_at"].as_i64().unwrap() - 20).into();
    std::fs::write(&start_path, start.to_string()).unwrap();
    sandbox
        .pmux()
        .args(["inspect", "agent", "--json"])
        .assert()
        .success()
        .stdout(predicates::str::contains("not_ready"));
}

/// Configured ready text confirms an idle agent at its prompt, which sends no
/// signal until its first turn; a startup dialog without that text does not.
#[test]
fn configured_ready_text_confirms_an_idle_agent_and_dialogs_do_not() {
    let Some(sandbox) = Sandbox::new("ready") else {
        return;
    };
    let bin = sandbox.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = bin.join("codex");
    // A trust dialog until the file "go" exists, then the composer prompt.
    let go = sandbox.home.path().join("go");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf 'Trust this folder?\\n> 1. Trust and continue\\n'\nwhile [ ! -e '{}' ]; do sleep 0.1; done\nclear\nprintf '> Ask Codex to do anything\\n'\nexec sleep 60\n",
            go.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let state = |sandbox: &Sandbox| -> serde_json::Value {
        let out = sandbox
            .pmux()
            .args(["inspect", "agent", "--json"])
            .assert()
            .success()
            .get_output()
            .clone();
        serde_json::from_slice(&out.stdout).unwrap()
    };
    let wait_startup = |sandbox: &Sandbox| {
        sandbox
            .pmux()
            .args([
                "wait",
                "agent",
                "--until",
                "agent:done",
                "--startup-timeout",
                "0s",
                "--timeout",
                "1s",
                "--json",
            ])
            .assert()
            .get_output()
            .clone()
    };

    sandbox
        .pmux()
        .args(["run", "-d", "-s", "agent", "codex"])
        .assert()
        .success();
    let config = sandbox.home.path().join(".config/pmux/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "[agents.codex]\nready_text = \"Ask Codex to do anything\"\n",
    )
    .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));

    // The dialog is not the prompt: still unconfirmed.
    let json = state(&sandbox);
    assert_ne!(
        json["startup"]["status"], "ready",
        "dialog read as ready: {json}"
    );
    assert_eq!(wait_startup(&sandbox).status.code(), Some(3));

    std::fs::write(&go, "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(800));
    let json = state(&sandbox);
    assert_eq!(json["startup"]["status"], "ready", "{json}");
    assert_eq!(json["startup"]["source"], "screen");
    assert!(
        json["agent"]["state"].is_null(),
        "readiness must not set agent state: {json}"
    );
    sandbox
        .pmux()
        .args(["inspect", "agent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent state: ready (prompt seen"));
    // Ready: the startup budget no longer applies, so this waits out --timeout.
    let out = wait_startup(&sandbox);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("timed_out"));

    // The match is kept for this launch, even once the text scrolls away.
    let start: serde_json::Value = serde_json::from_slice(
        &std::fs::read(sandbox.state_dir().join("agent_starts/agent")).unwrap(),
    )
    .unwrap();
    assert!(start["ready_at"].is_number(), "{start}");

    // Without the config, readiness is unconfirmed as before.
    std::fs::remove_file(&config).unwrap();
    assert_ne!(state(&sandbox)["startup"]["status"], "ready");
}

/// With ready text configured, ask must not type into a startup dialog: it
/// waits for the ready text and, when it never appears, sends nothing.
#[test]
fn ask_waits_for_ready_text_and_never_types_into_a_dialog() {
    let Some(sandbox) = Sandbox::new("askgate") else {
        return;
    };
    let bin = sandbox.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let go = sandbox.home.path().join("go");
    let typed = sandbox.home.path().join("typed");
    // A dialog that records any input, until "go" exists; then the prompt.
    std::fs::write(
        bin.join("codex"),
        format!(
            "#!/bin/sh\nprintf 'Trust this folder?\\n'\nwhile [ ! -e '{go}' ]; do if read -t 0.1 line; then printf '%s' \"$line\" > '{typed}'; fi; done\nclear\nprintf '> Ask Codex to do anything\\n'\nexec sleep 60\n",
            go = go.display(),
            typed = typed.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        bin.join("codex"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let config = sandbox.home.path().join(".config/pmux/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(
        &config,
        "[agents.codex]\nready_text = \"Ask Codex to do anything\"\n",
    )
    .unwrap();
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "agent", "codex"])
        .assert()
        .success();
    std::thread::sleep(std::time::Duration::from_millis(500));

    let out = sandbox
        .pmux()
        .args([
            "ask",
            "agent",
            "--startup-timeout",
            "1s",
            "--json",
            "do the thing",
        ])
        .assert()
        .code(3)
        .get_output()
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outcome"], "not_ready", "{json}");
    assert!(json["send"].is_null(), "ask typed into the dialog: {json}");
    assert_eq!(json["process"]["startup"]["ready_text_seen"], false);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("ready text is not on screen"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!typed.exists(), "the dialog received input");
    sandbox
        .pmux()
        .args(["inspect", "agent"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ready text not on screen"));

    // Once the prompt shows, ask sends (and then waits for done as usual).
    std::fs::write(&go, "").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let out = sandbox
        .pmux()
        .args(["ask", "agent", "--timeout", "1s", "--json", "now"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outcome"], "timed_out", "{json}");
    assert!(json["send"].is_object(), "{json}");
}

/// ls --cwd/--here keep processes whose current directory is that directory
/// or below it, in the table and in JSON.
#[test]
fn ls_filters_by_directory() {
    let Some(sandbox) = Sandbox::new("lscwd") else {
        return;
    };
    let root = sandbox.home.path().join("proj");
    let sub = root.join("sub");
    let other = sandbox.home.path().join("other");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    for (name, dir) in [
        ("in-root", root.clone()),
        ("in-sub", root.join("sub")),
        ("elsewhere", std::fs::canonicalize(&other).unwrap()),
    ] {
        sandbox
            .pmux()
            .args([
                "run",
                "-d",
                "-s",
                name,
                "--cwd",
                dir.to_str().unwrap(),
                "sleep",
                "60",
            ])
            .assert()
            .success();
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let table = sandbox
        .pmux()
        .args(["ls", "--cwd", root.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .clone();
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(
        table.contains("in-root") && table.contains("in-sub"),
        "{table}"
    );
    assert!(!table.contains("elsewhere"), "{table}");
    let json = sandbox
        .pmux()
        .current_dir(&root)
        .args(["ls", "--here", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let mut names: Vec<_> = json["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    assert_eq!(names, ["in-root", "in-sub"], "{json}");
}
