mod common;

use assert_cmd::Command;
use std::path::Path;
use std::time::{Duration, Instant};

fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn wait_for_lines(path: &Path, count: usize) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found = lines(path);
        if found.len() >= count || Instant::now() > deadline {
            return found;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

// `pmux respawn` restarts an agent in its own pane: the pane id survives, the
// agent is relaunched with its resume arguments, and it takes its environment
// from the server (which `pmux respawn` must not overwrite with the caller's).
#[test]
fn respawn_resumes_the_agent_in_the_same_pane_from_the_server_environment() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-rs-")
        .tempdir_in("/tmp")
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let bin = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let socket = format!("pmux-respawn-test-{}", std::process::id());

    // A stand-in agent named `claude` (so pmux detects it as one) that records
    // how it was started, then stays alive like an interactive agent.
    let fake = bin.path().join("claude");
    std::fs::write(
        &fake,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$PMUX_T_OUT/args\"\n\
         printf '%s\\n' \"${PMUX_T_MARK:-unset}\" >> \"$PMUX_T_OUT/mark\"\nexec sleep 300\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), common::path());
    let pmux = |mark: &str| {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env("PATH", &path)
            .env("PMUX_T_OUT", out.path())
            .env("PMUX_T_MARK", mark)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("CLAUDE_CONFIG_DIR")
            .current_dir(work.path());
        cmd
    };
    let tmux = |args: &[&str]| {
        std::process::Command::new("tmux")
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env_remove("TMUX")
            .arg("-L")
            .arg(&socket)
            .args(args)
            .output()
            .unwrap()
    };

    let started = pmux("at-spawn")
        .args(["run", "-d", "-s", "agent", "claude"])
        .output()
        .unwrap();
    let first = wait_for_lines(&out.path().join("args"), 1);
    let pane_before = tmux(&["display-message", "-p", "-t", "agent", "#{pane_id}"]);
    tmux(&["set-environment", "-g", "PMUX_T_MARK", "from-server"]);

    let respawned = pmux("from-caller")
        .args(["respawn", "agent", "--resume-id", "sess-42"])
        .output()
        .unwrap();
    let after_first = wait_for_lines(&out.path().join("args"), 2);
    // The first respawn recorded the identity, so a second one finds it.
    let dry_run = pmux("from-caller")
        .args(["respawn", "agent", "--dry-run"])
        .output()
        .unwrap();
    let again = pmux("from-caller")
        .args(["respawn", "agent"])
        .output()
        .unwrap();
    let after_second = wait_for_lines(&out.path().join("args"), 3);
    // New flags after `--` follow the resume arguments.
    let with_args_plan = pmux("from-caller")
        .args(["respawn", "agent", "--dry-run", "--", "--model", "opus x"])
        .output()
        .unwrap();
    let with_args = pmux("from-caller")
        .args(["respawn", "agent", "--", "--model", "opus x"])
        .output()
        .unwrap();
    let after_third = wait_for_lines(&out.path().join("args"), 4);
    let several = pmux("from-caller")
        .args(["respawn", "--all", "--", "--model", "x"])
        .output()
        .unwrap();
    let inspected = pmux("from-caller")
        .args(["inspect", "agent", "--json"])
        .output()
        .unwrap();
    let marks = lines(&out.path().join("mark"));
    let pane_after = tmux(&["display-message", "-p", "-t", "agent", "#{pane_id}"]);

    // Never leak a backend if an assertion below fails.
    tmux(&["kill-server"]);

    let stderr = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).to_string();
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(respawned.status.success(), "{}", stderr(&respawned));
    assert!(dry_run.status.success(), "{}", stderr(&dry_run));
    assert!(again.status.success(), "{}", stderr(&again));
    assert_eq!(
        first,
        vec![String::new()],
        "the agent starts with no arguments"
    );
    assert_eq!(
        after_first.get(1).map(String::as_str),
        Some("--resume sess-42")
    );
    assert_eq!(
        after_second.get(2).map(String::as_str),
        Some("--resume sess-42")
    );
    assert!(with_args.status.success(), "{}", stderr(&with_args));
    assert_eq!(
        after_third.get(3).map(String::as_str),
        Some("--resume sess-42 --model opus x")
    );
    let plan_with_args = String::from_utf8_lossy(&with_args_plan.stdout);
    assert!(
        plan_with_args.contains("resume: claude --resume sess-42 --model 'opus x'"),
        "{plan_with_args}"
    );
    let inspected: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(inspected["agent"]["session_id"], "sess-42");
    assert!(!several.status.success());
    assert!(stderr(&several).contains("exactly one named process"));
    assert_eq!(
        marks,
        vec!["at-spawn", "from-server", "from-server", "from-server"]
    );
    let pane_before = String::from_utf8_lossy(&pane_before.stdout)
        .trim()
        .to_string();
    assert!(pane_before.starts_with('%'));
    assert_eq!(
        String::from_utf8_lossy(&pane_after.stdout).trim(),
        pane_before,
        "respawn must keep the pane"
    );
    let plan = String::from_utf8_lossy(&dry_run.stdout);
    assert!(
        plan.contains("sess-42 (pmux records)") && plan.contains("resume"),
        "{plan}"
    );
    assert!(String::from_utf8_lossy(&respawned.stdout).contains("resumed claude session sess-42"));
}

// Several processes need --yes when there is no terminal to confirm on.
#[test]
fn respawn_refuses_several_processes_without_confirmation() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-rs-")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = tempfile::tempdir().unwrap();
    let socket = format!("pmux-respawn-confirm-{}", std::process::id());
    let fake = bin.path().join("claude");
    std::fs::write(&fake, "#!/bin/sh\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let path = format!("{}:{}", bin.path().display(), common::path());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env("PATH", &path)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        cmd
    };
    for name in ["one", "two"] {
        pmux()
            .args(["run", "-d", "-s", name, "claude"])
            .output()
            .unwrap();
    }
    let refused = pmux()
        .args(["respawn", "--agent", "claude", "--force"])
        .write_stdin("")
        .output()
        .unwrap();
    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .env_remove("TMUX")
        .args(["-L", &socket, "kill-server"])
        .status();

    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("rerun with --yes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    let plan = String::from_utf8_lossy(&refused.stdout);
    assert!(plan.contains("one") && plan.contains("two"), "{plan}");
}
