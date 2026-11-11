mod common;

use assert_cmd::Command;

// Regression: tmux format-expands `new-session -c`, so a working directory
// containing `#(...)` used to run that text as a shell command on the backend
// server. The directory must be used literally, `#` and all.
#[test]
fn run_cwd_with_format_sequence_is_taken_literally() {
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
    let work = tempfile::tempdir().unwrap();
    let work = work.path().canonicalize().unwrap();
    let marker = work.join("pwned");
    // `mkdir -p` of this path creates nested directories, so the hostile
    // directory really exists and the pane must start inside it.
    let hostile = work.join(format!("x#(touch {})", marker.display()));
    std::fs::create_dir_all(&hostile).unwrap();
    let hashed = work.join("a#b##c");
    std::fs::create_dir_all(&hashed).unwrap();

    let socket = format!("pmux-escape-test-{}", std::process::id());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX");
        cmd
    };
    let pane_path = |name: &str| {
        let output = std::process::Command::new("tmux")
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env_remove("TMUX")
            .args(["-L", &socket, "display-message", "-p", "-t"])
            .arg(format!("={name}:"))
            .arg("#{pane_current_path}")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    let hostile_run = pmux()
        .args(["run", "-d", "-s", "hostile", "--cwd"])
        .arg(&hostile)
        .args(["sleep", "60"])
        .output()
        .unwrap();
    let hashed_run = pmux()
        .args(["run", "-d", "-s", "hashed", "--cwd"])
        .arg(&hashed)
        .args(["sleep", "60"])
        .output()
        .unwrap();
    // `#()` jobs run asynchronously; give one time to fire.
    std::thread::sleep(std::time::Duration::from_millis(800));
    let hostile_path = pane_path("hostile");
    let hashed_path = pane_path("hashed");
    let executed = marker.exists();

    // Never leak a backend if an assertion below fails.
    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .env_remove("TMUX")
        .args(["-L", &socket, "kill-server"])
        .status();

    assert!(
        hostile_run.status.success(),
        "{}",
        String::from_utf8_lossy(&hostile_run.stderr)
    );
    assert!(
        hashed_run.status.success(),
        "{}",
        String::from_utf8_lossy(&hashed_run.stderr)
    );
    assert!(!executed, "tmux ran the #(...) in the working directory");
    assert_eq!(hostile_path, hostile.display().to_string());
    assert_eq!(hashed_path, hashed.display().to_string());
}
