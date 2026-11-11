//! Screen capture must preserve the entire approval box, even below the cursor.

mod common;

use assert_cmd::Command;
use std::time::{Duration, Instant};

#[test]
fn capture_preserves_bottom_prompt_boxes_in_normal_and_alternate_screens() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    for alternate in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let socket_dir = tempfile::Builder::new()
            .prefix("pmux-cap-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = format!("pmux-capture-{}-{}", std::process::id(), alternate);
        struct Server<'a>(&'a std::path::Path, &'a str);
        impl Server<'_> {
            fn tmux(&self) -> std::process::Command {
                let mut cmd = std::process::Command::new("tmux");
                cmd.env("TMUX_TMPDIR", self.0).env_remove("TMUX").args([
                    "-L",
                    self.1,
                    "-f",
                    "/dev/null",
                ]);
                cmd
            }
        }
        impl Drop for Server<'_> {
            fn drop(&mut self) {
                let _ = self.tmux().arg("kill-server").output();
            }
        }
        let server = Server(socket_dir.path(), &socket);
        let pmux = || {
            let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
            cmd.env("HOME", home.path())
                .env("PATH", common::path())
                .env("PMUX_SOCKET", &socket)
                .env("TMUX_TMPDIR", socket_dir.path())
                .env_remove("TMUX")
                .env_remove("TMUX_PANE");
            cmd
        };
        let script = home.path().join("approval.py");
        std::fs::write(&script, r#"
import os, sys, time
# Establish real history before showing the prompt.
os.write(1, ''.join('history-%02d\r\n' % i for i in range(30)).encode())
if sys.argv[1] == 'true': os.write(1, b'\x1b[?1049h\x1b[2J\x1b[H')
box = ['┌─ Approval required ─┐', '│ Run cargo test?    │', '│ [y] Allow [n] Deny │', '└────────────────────┘']
# Draw to the bottom row and leave the cursor inside the dialog, above its end.
os.write(1, ('\x1b[9;1H\x1b[33m' + '\r\n'.join(box) + '\x1b[0m\x1b[10;4H').encode())
time.sleep(60)
"#).unwrap();
        assert!(
            server
                .tmux()
                .args([
                    "new-session",
                    "-d",
                    "-s",
                    "approval",
                    "-x",
                    "80",
                    "-y",
                    "12",
                    "python3"
                ])
                .arg(&script)
                .arg(alternate.to_string())
                .status()
                .unwrap()
                .success()
        );
        let box_text = "┌─ Approval required ─┐\n│ Run cargo test?    │\n│ [y] Allow [n] Deny │\n└────────────────────┘\n";
        let started = Instant::now();
        let screen = loop {
            let out = pmux().args(["capture", "approval"]).output().unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8(out.stdout).unwrap();
            if text.ends_with(box_text) {
                break text;
            }
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "missing approval box (alternate={alternate}): {text:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(screen.lines().count(), 12);
        assert!(!screen.contains('\x1b'));
        let out = pmux()
            .args(["capture", "newest", "--lines", "4"])
            .assert()
            .success()
            .get_output()
            .clone();
        assert_eq!(out.stdout, box_text.as_bytes());
        let out = pmux()
            .args(["capture", "oldest", "--history", "--json"])
            .assert()
            .success()
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["api_version"], 1);
        assert_eq!(json["session"], "approval");
        assert!(json["pane_id"].as_str().unwrap().starts_with('%'));
        let history = json["text"].as_str().unwrap();
        assert!(history.ends_with(box_text));
        if !alternate {
            assert!(history.contains("history-00"));
            assert!(!screen.contains("history-00"));
            let out = pmux()
                .args(["capture", "approval", "--lines", "15"])
                .assert()
                .success()
                .get_output()
                .clone();
            let text = String::from_utf8(out.stdout).unwrap();
            assert_eq!(text.lines().count(), 15);
            assert!(text.ends_with(box_text));
        }
        pmux().args(["capture", "missing"]).assert().failure();
        pmux()
            .args(["capture", "approval", "--lines", "0"])
            .assert()
            .failure();
        pmux()
            .args(["capture", "approval", "--lines", "4", "--history"])
            .assert()
            .failure();
        // Capture must not change the visible screen or inject input.
        let out = pmux()
            .args(["capture", "approval"])
            .assert()
            .success()
            .get_output()
            .clone();
        assert_eq!(out.stdout, screen.as_bytes());
        let buffers = server.tmux().arg("list-buffers").output().unwrap();
        assert!(
            buffers.stdout.is_empty(),
            "capture changed tmux paste buffers"
        );
    }
}
