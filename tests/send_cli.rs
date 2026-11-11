mod common;

use assert_cmd::Command;

// Regression: `pmux send` passed the text to `tmux send-keys -l` without
// `--`, so text starting with `-` was parsed as send-keys flags: `-R` was
// silently swallowed and `- fix bug` failed with "invalid flag".
#[test]
fn send_delivers_text_that_starts_with_a_dash() {
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
    let received = out.path().join("received");
    let socket = format!("pmux-send-test-{}", std::process::id());
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX");
        cmd
    };

    // `cat` copies each submitted line to the file, so the file holds exactly
    // what tmux typed into the pane.
    let started = pmux()
        .args(["run", "-d", "-s", "reader", "sh", "-c"])
        .arg(format!("stty -echo; cat > '{}'", received.display()))
        .output()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    let first = pmux().args(["send", "reader", "-R"]).output().unwrap();
    let second = pmux()
        .args(["send", "reader", "- fix bug"])
        .output()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500));
    let content = std::fs::read_to_string(&received).unwrap_or_default();

    // Never leak a backend if an assertion below fails.
    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .env_remove("TMUX")
        .args(["-L", &socket, "kill-server"])
        .status();

    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(content, "-R\n- fix bug\n");
}

// A raw-mode fake TUI advertises bracketed paste and records the terminal
// protocol, including the gap before Enter. No real agent or live server.
#[test]
fn send_frames_pastes_preserves_approval_keys_and_sends_enter_separately() {
    use std::time::{Duration, Instant};
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-paste-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = format!("pmux-paste-{}", std::process::id());
    struct Server<'a>(&'a std::path::Path, &'a str);
    impl Server<'_> {
        fn tmux(&self) -> std::process::Command {
            let mut command = std::process::Command::new("tmux");
            command
                .env("TMUX_TMPDIR", self.0)
                .env_remove("TMUX")
                .args(["-L", self.1]);
            command
        }
    }
    impl Drop for Server<'_> {
        fn drop(&mut self) {
            let _ = self.tmux().arg("kill-server").output();
        }
    }
    let server = Server(tmux_tmp.path(), &socket);
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env("HOME", home.path())
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE");
        cmd
    };
    let script = home.path().join("fake_tui.py");
    std::fs::write(
        &script,
        r#"
import os, sys, tty, time, pathlib, json
directory = pathlib.Path(sys.argv[1])
tty.setraw(0)
os.write(1, b'\x1b[?2004h')
(directory / 'ready').touch()
data = b''
paste_end = None
enters = []
while True:
    chunk = os.read(0, 65536)
    if not chunk: break
    for byte in chunk:
        char = bytes([byte])
        data += char
        if data.endswith(b'\x1b[201~'): paste_end = time.monotonic()
        if char == b'\r':
            enters.append(None if paste_end is None else time.monotonic() - paste_end)
    (directory / 'enters').write_text(json.dumps(enters))
    (directory / 'received').write_bytes(data)
"#,
    )
    .unwrap();
    pmux()
        .args(["run", "-d", "-s", "reader", "python3", "-u"])
        .arg(&script)
        .arg(home.path())
        .assert()
        .success();
    let wait_for = |file: &str| {
        let started = Instant::now();
        while !home.path().join(file).exists() {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "fake TUI did not create {file}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for("ready");
    assert!(
        server
            .tmux()
            .args(["set-buffer", "-b", "user-buffer", "clipboard"])
            .status()
            .unwrap()
            .success()
    );
    pmux()
        .args(["send", "reader", "--no-enter", "p"])
        .assert()
        .success();
    pmux()
        .args(["send", "reader", "--no-enter", "y"])
        .assert()
        .success();
    let prompt = "x".repeat(916);
    pmux()
        .args(["send", "reader", &prompt])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "sent Enter (application submission not confirmed)",
        ));
    let multiline = "-R\nquotes ' \" \\ 表🙂";
    pmux()
        .args(["send", "reader", "--stdin", "--json"])
        .write_stdin(multiline)
        .assert()
        .success()
        .stdout(predicates::str::contains("\"submission_confirmed\": false"));
    // Empty input remains a plain Enter, without an empty paste frame.
    pmux().args(["send", "reader", ""]).assert().success();
    let expected =
        format!("py\x1b[200~{prompt}\x1b[201~\r\x1b[200~{multiline}\x1b[201~\r\r").into_bytes();
    let started = Instant::now();
    loop {
        let received = std::fs::read(home.path().join("received")).unwrap_or_default();
        if received == expected {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "unexpected terminal input: {received:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    pmux()
        .args([
            "send", "reader", "--key", "BSpace", "--repeat", "12", "--json",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"keys_sent\": 12"));
    pmux()
        .args(["send", "reader", "--key", "C-u", "--key", "Escape"])
        .assert()
        .success();
    pmux()
        .args(["send", "reader", "--key", "Escap"])
        .assert()
        .failure();
    let mut expected_keys = expected;
    expected_keys.extend([127; 12]);
    expected_keys.extend([21, 27]);
    let started = Instant::now();
    while std::fs::read(home.path().join("received")).unwrap_or_default() != expected_keys {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "named keys did not arrive literally as terminal key codes"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let long_prompt = format!(
        "private-file-marker\n{}\nlast line",
        "long prompt ' \" \\ 表🙂 ".repeat(256)
    );
    let prompt_path = home.path().join("prompt.txt");
    std::fs::write(&prompt_path, &long_prompt).unwrap();
    pmux()
        .args(["send", "reader", "--prompt-file"])
        .arg(&prompt_path)
        .assert()
        .success();
    let mut expected_file = expected_keys;
    expected_file.extend(format!("\x1b[200~{long_prompt}\x1b[201~\r").as_bytes());
    let started = Instant::now();
    while std::fs::read(home.path().join("received")).unwrap_or_default() != expected_file {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "prompt file was not delivered exactly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(std::fs::read_to_string(&prompt_path).unwrap(), long_prompt);
    let inspect = pmux()
        .args(["inspect", "reader", "--json"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!String::from_utf8_lossy(&inspect.stdout).contains("private-file-marker"));
    let invalid = home.path().join("bad-prompt");
    std::fs::write(&invalid, [255]).unwrap();
    pmux()
        .args(["send", "reader", "--prompt-file"])
        .arg(&invalid)
        .assert()
        .failure();
    std::fs::write(&invalid, b"bad\0prompt").unwrap();
    pmux()
        .args(["send", "reader", "--prompt-file"])
        .arg(&invalid)
        .assert()
        .failure();
    pmux()
        .args(["send", "reader", "--prompt-file", "/no/such/pmux-prompt"])
        .assert()
        .failure();
    assert_eq!(
        std::fs::read(home.path().join("received")).unwrap(),
        expected_file
    );
    let gaps: Vec<f64> =
        serde_json::from_slice(&std::fs::read(home.path().join("enters")).unwrap()).unwrap();
    assert!(
        gaps[0] >= 0.05 && gaps[1] >= 0.05,
        "Enter raced paste: {gaps:?}"
    );
    let clipboard = server
        .tmux()
        .args(["show-buffer", "-b", "user-buffer"])
        .output()
        .unwrap();
    assert_eq!(clipboard.stdout, b"clipboard");
    let buffers = server
        .tmux()
        .args(["list-buffers", "-F", "#{buffer_name}"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&buffers.stdout).contains("pmux-send-"));
}
