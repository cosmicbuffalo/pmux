mod common;

use assert_cmd::Command;
use predicates::str::contains;

// Regression: `restart --hard` used to prompt on stdin even without a
// terminal. Scripts piping stdin hung forever; scripts with stdin at EOF got
// "Aborted." with exit 0 while the server (and its sessions) kept running.
#[test]
fn hard_restart_without_a_terminal_requires_yes() {
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
    let socket = format!("pmux-restart-test-{}", std::process::id());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX");
        cmd
    };
    let server_alive = || {
        std::process::Command::new("tmux")
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .args(["-L", &socket, "has-session"])
            .status()
            .is_ok_and(|status| status.success())
    };

    pmux()
        .args(["run", "-d", "-s", "keep", "sleep", "60"])
        .assert()
        .success();

    pmux()
        .args(["restart", "--hard"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(contains("--yes"));
    let survived = server_alive();

    pmux()
        .args(["restart", "--hard", "--yes"])
        .write_stdin("")
        .assert()
        .success();
    let stopped = !server_alive();

    // Never leak a backend if an assertion above fails.
    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .args(["-L", &socket, "kill-server"])
        .status();
    assert!(survived, "a refused restart must not stop the server");
    assert!(stopped, "--yes must stop the server");
}
