//! Wait regressions use a private tmux socket, HOME, and ai-attn directory.

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
            .env("PATH", common::path())
            .env("TMUX_TMPDIR", self.tmux_tmp.path())
            .env("PMUX_SOCKET", &self.socket)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env("AI_ATTN_STATE_DIR", self.home.path().join("signals"))
            .env("CODEX_HOME", self.home.path().join(".codex"));
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

fn signal(sandbox: &Sandbox, state: &str) {
    signal_reason(sandbox, state, "PermissionRequest");
}

fn signal_reason(sandbox: &Sandbox, state: &str, reason: &str) {
    let dir = sandbox.home.path().join("signals");
    std::fs::create_dir_all(&dir).unwrap();
    let value = serde_json::json!({"agent":"codex", "state":state, "reason":reason, "updated_at":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()});
    std::fs::write(dir.join("next"), value.to_string()).unwrap();
    std::fs::rename(dir.join("next"), dir.join("agent.json")).unwrap();
}

#[test]
fn waits_stop_for_input_match_alternatives_and_resume_after_answering() {
    let Some(sandbox) = Sandbox::new("wait") else {
        return;
    };
    sandbox
        .pmux()
        .args([
            "run",
            "-d",
            "-s",
            "probe",
            "sh",
            "-c",
            "printf 'Allow test command? [y/n]\\n'; exec sleep 60",
        ])
        .assert()
        .success();
    // Classify the fake agent via its launch metadata; never launch a real AI CLI.
    sandbox.plant("launch_cmds", "probe", "codex\n");
    signal(&sandbox, "working");
    std::thread::scope(|scope| {
        let update = scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal(&sandbox, "waiting");
        });
        let out = sandbox
            .pmux()
            .args([
                "wait",
                "probe",
                "--until",
                "agent:done",
                "--json",
                "--timeout",
                "3s",
            ])
            .timeout(std::time::Duration::from_secs(5))
            .assert()
            .code(2)
            .get_output()
            .clone();
        update.join().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["outcome"], "blocked");
        assert_eq!(json["process"]["agent"]["state"], "waiting");
        assert_eq!(json["process"]["agent"]["reason"], "PermissionRequest");
        assert!(String::from_utf8_lossy(&out.stderr).contains("PermissionRequest"));
    });
    for (flag, rows) in [("--capture-on-block", 80), ("--capture-on-block=2", 2)] {
        let out = sandbox
            .pmux()
            .args(["wait", "probe", "--until", "agent:done", "--json", flag])
            .assert()
            .code(2)
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["outcome"], "blocked");
        assert_eq!(json["capture"]["session"], "probe");
        if rows == 80 {
            assert!(
                json["capture"]["text"]
                    .as_str()
                    .unwrap()
                    .contains("Allow test command? [y/n]")
            );
        }
        assert_eq!(json["capture"]["pane_id"], json["process"]["pane_id"]);
        assert!(json["capture"]["text"].as_str().unwrap().lines().count() <= rows);
    }
    sandbox
        .pmux()
        .args([
            "wait",
            "probe",
            "--until",
            "agent:done",
            "--capture-on-block=2",
        ])
        .assert()
        .code(2)
        .stdout(predicates::str::contains("blocked"));
    sandbox
        .pmux()
        .args(["wait", "probe", "--capture-on-block=0"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("invalid value"));
    for args in [
        vec![
            "--until",
            "agent:done",
            "--until",
            "agent:waiting",
            "--capture-on-block",
        ],
        vec!["--until", "agent:done,agent:waiting"],
    ] {
        let out = sandbox
            .pmux()
            .args(["wait", "probe", "--json", "--timeout", "1s"])
            .args(args)
            .assert()
            .success()
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["outcome"], "matched");
        assert_eq!(json["matched_condition"], "agent:waiting");
        assert!(json.get("capture").is_none());
    }
    // An exit-only wait keeps waiting while an agent needs input.
    sandbox
        .pmux()
        .args(["wait", "probe", "--timeout", "1ms", "--json"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("timed_out"));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal(&sandbox, "working"); // simulate answering the approval
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal(&sandbox, "done");
        });
        sandbox
            .pmux()
            .args([
                "wait",
                "probe",
                "--until",
                "agent:done",
                "--ignore-waiting",
                "--json",
                "--timeout",
                "3s",
            ])
            .timeout(std::time::Duration::from_secs(5))
            .assert()
            .success()
            .stdout(predicates::str::contains("agent:done"));
    });
    sandbox
        .pmux()
        .args([
            "wait",
            "probe",
            "--until",
            "agent:done",
            "--json",
            "--timeout",
            "1s",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("matched_condition"));
    sandbox
        .pmux()
        .args([
            "wait",
            "probe",
            "--until",
            "agent:done,",
            "--timeout",
            "1ms",
        ])
        .assert()
        .failure();
}

#[test]
fn ask_composes_send_wait_reply_and_does_not_reuse_old_completion() {
    let Some(sandbox) = Sandbox::new("ask") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "probe", "cat"])
        .assert()
        .success();
    sandbox.plant("launch_cmds", "probe", "codex\n");
    let info = sandbox
        .pmux()
        .args(["inspect", "probe", "--json"])
        .output()
        .unwrap();
    let info: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
    sandbox.plant(
        "agent_sessions",
        "probe",
        &format!("codex\tnative-id\t{}\n", info["created_at"]),
    );
    let transcript = sandbox
        .home
        .path()
        .join(".codex/sessions/2020/01/01/rollout-native-id.jsonl");
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    let write_reply = |text: &str| {
        let record = serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "channel":"final", "content":[{"type":"output_text", "text":text}]}});
        std::fs::write(&transcript, format!("{record}\n")).unwrap();
    };
    write_reply("old reply");
    signal(&sandbox, "done");
    // Unchanged done must time out rather than returning the old transcript.
    sandbox
        .pmux()
        .args(["ask", "probe", "--timeout", "1ms", "--json", "first prompt"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("timed_out"));
    let prompt = sandbox.home.path().join("prompt.txt");
    std::fs::write(&prompt, "multiline\nprompt λ").unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            signal(&sandbox, "working");
            std::thread::sleep(std::time::Duration::from_millis(400));
            write_reply("new reply\nsecond line");
            signal(&sandbox, "done");
        });
        let out = sandbox
            .pmux()
            .args([
                "ask",
                "newest",
                "--prompt-file",
                prompt.to_str().unwrap(),
                "--json",
                "--compact",
                "--timeout",
                "3s",
            ])
            .timeout(std::time::Duration::from_secs(5))
            .assert()
            .success()
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["outcome"], "matched");
        assert_eq!(json["matched_condition"], "agent:done");
        assert_eq!(json["send"]["bytes_sent"], "multiline\nprompt λ".len());
        assert_eq!(json["reply"]["text"], "new reply\nsecond line");
        assert!(json["process"].get("start_command").is_none());
    });
    // A new done with no new transcript must not silently print an old answer.
    signal(&sandbox, "working");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            signal(&sandbox, "done");
        });
        sandbox
            .pmux()
            .args(["ask", "probe", "--stdin", "--timeout", "3s"])
            .write_stdin("next prompt")
            .assert()
            .code(1)
            .stderr(predicates::str::contains("unchanged"));
    });
    signal(&sandbox, "waiting");
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal_reason(&sandbox, "waiting", "NewPermissionRequest");
        });
        sandbox
            .pmux()
            .args([
                "ask",
                "probe",
                "--json",
                "--capture-on-block",
                "blocked prompt",
            ])
            .assert()
            .code(2)
            .stdout(predicates::str::contains("\"capture\""));
    });
    std::fs::remove_dir_all(sandbox.home.path().join("signals")).unwrap();
    sandbox
        .pmux()
        .args([
            "ask",
            "probe",
            "--startup-timeout",
            "0s",
            "--json",
            "startup prompt",
        ])
        .assert()
        .code(3)
        .stdout(predicates::str::contains("not_ready"));
    sandbox
        .pmux()
        .args(["capture", "probe"])
        .assert()
        .success()
        .stdout(predicates::str::contains("multiline"));
    for args in [
        vec!["ask", "probe", "--stdin", "--prompt-file", "file"],
        vec!["ask", "probe"],
    ] {
        sandbox.pmux().args(args).assert().code(2);
    }
}

#[test]
fn ask_reports_fast_exit_as_a_wait_outcome() {
    let Some(sandbox) = Sandbox::new("ask-exit") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "probe", "sh", "-c", "read -r prompt"])
        .assert()
        .success();
    sandbox.plant("launch_cmds", "probe", "codex\n");
    signal(&sandbox, "done");
    let out = sandbox
        .pmux()
        .args(["ask", "probe", "--json", "--timeout", "3s", "finish"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["outcome"], "exited");
    assert_eq!(json["send"]["enter_sent"], true);
    assert!(json["process"].is_null());
    assert!(json["reply"].is_null());
}

#[test]
fn ask_human_blocked_output_captures_screen_with_options_after_prompt() {
    let Some(sandbox) = Sandbox::new("ask-blocked-human") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "probe", "sh", "-c"])
        .arg(format!(
            "stty -echo; printf '{}Approve test command? [y/n]\\n'; exec sleep 60",
            "\\n".repeat(30)
        ))
        .assert()
        .success();
    sandbox.plant("launch_cmds", "probe", "codex\n");
    // Wait for the fake approval screen, not for an arbitrary startup delay.
    let expected = (0..100)
        .find_map(|_| {
            let output = sandbox
                .pmux()
                .args(["capture", "probe", "--lines", "10"])
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            if text.contains("Approve test command? [y/n]") {
                Some(text)
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
                None
            }
        })
        .expect("fake approval screen did not appear");
    signal(&sandbox, "waiting");
    for (index, args) in [
        vec![
            "ask",
            "probe",
            "Run curl -sI https://example.com",
            "--timeout",
            "3s",
            "--capture-on-block=10",
        ],
        vec![
            "ask",
            "probe",
            "--timeout",
            "3s",
            "--capture-on-block=10",
            "Run curl -sI https://example.com",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(350));
                signal_reason(&sandbox, "waiting", &format!("PermissionRequest {index}"));
            });
            sandbox
                .pmux()
                .args(args)
                .assert()
                .code(2)
                .stdout(predicates::str::contains(
                    "probe: blocked while waiting for agent:done",
                ))
                .stdout(predicates::str::contains(expected.clone()))
                .stderr(predicates::str::contains(
                    "pmux wait probe --until agent:done",
                ))
                .stderr(predicates::str::contains("pmux reply probe"));
        });
    }
    // JSON flags after the literal prompt must also remain options.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal_reason(&sandbox, "waiting", "PermissionRequest JSON");
        });
        let out = sandbox
            .pmux()
            .args([
                "ask",
                "probe",
                "prompt",
                "--timeout",
                "3s",
                "--capture-on-block=10",
                "--json",
            ])
            .assert()
            .code(2)
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["capture"]["text"], expected);
        assert_eq!(json["send"]["bytes_sent"], "prompt".len());
    });
    sandbox
        .pmux()
        .args([
            "wait",
            "probe",
            "--until",
            "agent:done",
            "--capture-on-block=10",
        ])
        .assert()
        .code(2)
        .stdout(predicates::str::contains(
            "probe: blocked while waiting for agent:done",
        ))
        .stdout(predicates::str::contains(expected));
}

#[test]
fn delivered_input_supersedes_old_approval_but_new_requests_still_block() {
    let Some(sandbox) = Sandbox::new("answered-approval") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "probe", "sleep", "60"])
        .assert()
        .success();
    sandbox.plant("launch_cmds", "probe", "codex\n");
    for mode in ["literal", "key", "ask"] {
        signal(&sandbox, "waiting");
        match mode {
            "literal" => {
                sandbox
                    .pmux()
                    .args(["send", "probe", "--no-enter", "y"])
                    .assert()
                    .success();
            }
            "key" => {
                sandbox
                    .pmux()
                    .args(["send", "probe", "--key", "Enter"])
                    .assert()
                    .success();
            }
            "ask" => {
                sandbox
                    .pmux()
                    .args(["ask", "probe", "continue", "--timeout", "100ms", "--json"])
                    .assert()
                    .code(1)
                    .stdout(predicates::str::contains("timed_out"));
            }
            _ => unreachable!(),
        }
        let path = sandbox.state_dir().join("last_inputs/probe");
        let receipt = std::fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&receipt).unwrap();
        assert!(value["delivered_at"].as_i64().unwrap() > 0);
        // No new signal on approval acceptance: the raw state stays waiting.
        for until in ["agent:done", "agent:waiting"] {
            sandbox
                .pmux()
                .args([
                    "wait",
                    "probe",
                    "--until",
                    until,
                    "--timeout",
                    "100ms",
                    "--json",
                ])
                .assert()
                .code(1)
                .stdout(predicates::str::contains("timed_out"));
        }
        // Receipts follow rename and cannot acknowledge a replacement pane.
        if mode == "literal" {
            sandbox
                .pmux()
                .args(["rename-session", "probe", "renamed"])
                .assert()
                .success();
            sandbox
                .pmux()
                .args([
                    "wait",
                    "renamed",
                    "--until",
                    "agent:done",
                    "--timeout",
                    "1ms",
                    "--json",
                ])
                .assert()
                .code(1)
                .stdout(predicates::str::contains("timed_out"));
            sandbox
                .pmux()
                .args(["rename-session", "renamed", "probe"])
                .assert()
                .success();
            let mut replacement = value;
            replacement["pane_id"] = "%other-pane".into();
            std::fs::write(&path, replacement.to_string()).unwrap();
            sandbox
                .pmux()
                .args([
                    "wait",
                    "probe",
                    "--until",
                    "agent:done",
                    "--timeout",
                    "1ms",
                    "--json",
                ])
                .assert()
                .code(2)
                .stdout(predicates::str::contains("blocked"));
            std::fs::write(&path, &receipt).unwrap();
        }
        // A distinct request, including within the same second, must still block.
        signal_reason(&sandbox, "waiting", "NewPermissionRequest");
        sandbox
            .pmux()
            .args([
                "wait",
                "probe",
                "--until",
                "agent:done",
                "--timeout",
                "1s",
                "--json",
            ])
            .assert()
            .code(2)
            .stdout(predicates::str::contains("blocked"));
        // Rejected keys and empty no-enter sends cannot acknowledge that request.
        sandbox
            .pmux()
            .args(["send", "probe", "--key", "NotAKey"])
            .assert()
            .code(1);
        sandbox
            .pmux()
            .args(["send", "probe", "--no-enter", ""])
            .assert()
            .success();
        assert_eq!(std::fs::read(&path).unwrap(), receipt);
        sandbox
            .pmux()
            .args([
                "wait",
                "probe",
                "--until",
                "agent:waiting",
                "--timeout",
                "1s",
                "--json",
            ])
            .assert()
            .success()
            .stdout(predicates::str::contains("matched"));
    }
    sandbox
        .pmux()
        .args(["send", "probe", "--no-enter", "y"])
        .assert()
        .success();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Simulate the approved command running with no intermediate hook.
            std::thread::sleep(std::time::Duration::from_millis(350));
            signal(&sandbox, "done");
        });
        sandbox
            .pmux()
            .args([
                "wait",
                "probe",
                "--until",
                "agent:done",
                "--timeout",
                "3s",
                "--json",
            ])
            .assert()
            .success()
            .stdout(predicates::str::contains("matched"));
    });
}

#[test]
fn blocked_capture_settles_when_signal_precedes_approval_drawing() {
    for (command, draw_prompt) in [("wait", true), ("ask", true), ("wait", false)] {
        let Some(sandbox) = Sandbox::new(&format!("draw-after-signal-{command}-{draw_prompt}"))
        else {
            return;
        };
        let draw = sandbox.home.path().join("draw-approval");
        sandbox.pmux().args(["run", "-d", "-s", "probe", "sh", "-c"])
            .arg(format!(
                "stty -echo; printf 'Working\\n'; while [ ! -f '{}' ]; do sleep 0.01; done; printf 'Approve delayed command? [y/n]\\n'; exec sleep 60",
                draw.display()
            ))
            .assert().success();
        sandbox.plant("launch_cmds", "probe", "codex\n");
        signal(&sandbox, "working");
        let initial = sandbox.pmux().args(["capture", "probe"]).output().unwrap();
        assert!(!String::from_utf8_lossy(&initial.stdout).contains("Approve delayed command?"));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(350));
                signal(&sandbox, "waiting");
                // Reproduce Codex's observed hook-before-draw ordering.
                if draw_prompt {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    std::fs::write(&draw, "draw now").unwrap();
                }
            });
            let mut args = vec![
                command,
                "probe",
                "--capture-on-block",
                "--json",
                "--timeout",
                "3s",
            ];
            if command == "wait" {
                args.extend(["--until", "agent:done"]);
            } else {
                args.push("trigger approval");
            }
            let out = sandbox
                .pmux()
                .args(args)
                .timeout(std::time::Duration::from_secs(8))
                .assert()
                .code(2)
                .get_output()
                .clone();
            let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            assert_eq!(json["outcome"], "blocked");
            let text = json["capture"]["text"].as_str().unwrap();
            assert!(text.contains("Working"));
            assert_eq!(text.contains("Approve delayed command? [y/n]"), draw_prompt);
            // The no-draw case must still return within the command deadline;
            // settling must not wait indefinitely for a prompt or screen change.
        });
    }
}

#[test]
fn send_confirm_waits_for_a_fresh_signal_after_the_send() {
    let Some(sandbox) = Sandbox::new("send-confirm") else {
        return;
    };
    sandbox
        .pmux()
        .args(["run", "-d", "-s", "probe", "cat"])
        .assert()
        .success();
    sandbox.plant("launch_cmds", "probe", "codex\n");

    // The signal from before the send cannot confirm it.
    signal(&sandbox, "done");
    sandbox
        .pmux()
        .args(["send", "probe", "--confirm=300ms", "--json", "stale"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("\"timed_out\""))
        .stdout(predicates::str::contains("\"submission_confirmed\": false"));

    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            signal_reason(&sandbox, "working", "UserPromptSubmit");
        });
        let out = sandbox
            .pmux()
            .args(["send", "probe", "--confirm=5s", "--json", "--compact", "go"])
            .timeout(std::time::Duration::from_secs(8))
            .assert()
            .success()
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["outcome"], "matched");
        assert_eq!(json["matched_condition"], "agent:working");
        assert_eq!(json["send"]["submission_confirmed"], true);
        assert_eq!(json["send"]["bytes_sent"], 2);
    });

    // A question shown right after the send blocks, with the screen.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(1100));
            signal_reason(&sandbox, "waiting", "PermissionRequest");
        });
        sandbox
            .pmux()
            .args(["send", "probe", "--confirm", "--capture-on-block", "ask me"])
            .timeout(std::time::Duration::from_secs(10))
            .assert()
            .code(2)
            .stdout(predicates::str::contains("ask me"));
    });

    // No signal at all by the timeout: not_ready, with the screen when asked.
    std::fs::remove_dir_all(sandbox.home.path().join("signals")).unwrap();
    sandbox
        .pmux()
        .args([
            "send",
            "probe",
            "--confirm=500ms",
            "--capture-on-block=5",
            "--json",
            "hello",
        ])
        .assert()
        .code(3)
        .stdout(predicates::str::contains("\"not_ready\""))
        .stdout(predicates::str::contains("\"capture\""));

    // A fresh agent past its launch startup budget, with no signal yet, is
    // still credited when its first turn's signal follows the send.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            signal_reason(&sandbox, "working", "UserPromptSubmit");
        });
        sandbox
            .pmux()
            .args(["send", "probe", "--confirm=5s", "--json", "first brief"])
            .timeout(std::time::Duration::from_secs(8))
            .assert()
            .success()
            .stdout(predicates::str::contains("\"matched\""));
    });

    for args in [
        vec!["send", "probe", "--confirm", "--key", "Escape"],
        vec!["send", "probe", "--confirm", "--no-enter", "x"],
        vec!["send", "probe", "--capture-on-block", "x"],
    ] {
        sandbox.pmux().args(args).assert().code(2);
    }
}

#[test]
fn wait_on_several_processes_with_all_or_any() {
    let Some(sandbox) = Sandbox::new("wait-many") else {
        return;
    };
    let mut panes = Vec::new();
    for name in ["a", "b"] {
        sandbox
            .pmux()
            .args(["run", "-d", "-s", name, "cat"])
            .assert()
            .success();
        sandbox.plant("launch_cmds", name, "codex\n");
        let info = sandbox
            .pmux()
            .args(["inspect", name, "--json"])
            .output()
            .unwrap();
        let info: serde_json::Value = serde_json::from_slice(&info.stdout).unwrap();
        panes.push(info["pane_id"].as_str().unwrap().to_string());
    }
    let pane_signal = |index: usize, state: &str| {
        let dir = sandbox.home.path().join("signals");
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let value = serde_json::json!({"agent":"codex", "state":state, "updated_at":now, "pane_id":panes[index]});
        let next = dir.join(format!("next{index}"));
        std::fs::write(&next, value.to_string()).unwrap();
        std::fs::rename(&next, dir.join(format!("agent{index}.json"))).unwrap();
    };
    let wait = |args: &[&str]| {
        let out = sandbox
            .pmux()
            .args([
                "wait",
                "a",
                "b",
                "--until",
                "agent:done",
                "--json",
                "--compact",
            ])
            .args(args)
            .timeout(std::time::Duration::from_secs(8))
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.code(), json)
    };
    let outcomes = |json: &serde_json::Value| -> Vec<String> {
        json["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                format!(
                    "{}={}",
                    r["session"].as_str().unwrap(),
                    r["outcome"].as_str().unwrap()
                )
            })
            .collect()
    };

    pane_signal(0, "done");
    pane_signal(1, "working");
    let (code, json) = wait(&["--any"]);
    assert_eq!(code, Some(0));
    assert_eq!(json["mode"], "any");
    assert_eq!(json["decided_by"], "a");
    assert_eq!(outcomes(&json), ["a=matched", "b=pending"]);
    assert!(json["results"][0]["process"].get("start_command").is_none());

    // --all keeps waiting for b, then reports both.
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            pane_signal(1, "done");
        });
        let (code, json) = wait(&["--all", "--timeout", "5s"]);
        assert_eq!(code, Some(0));
        assert_eq!(json["outcome"], "matched");
        assert!(json["decided_by"].is_null());
        assert_eq!(outcomes(&json), ["a=matched", "b=matched"]);
    });

    // One process needing input ends an --all wait at once.
    pane_signal(1, "waiting");
    let (code, json) = wait(&["--all", "--timeout", "5s"]);
    assert_eq!(code, Some(2));
    assert_eq!(json["outcome"], "blocked");
    assert_eq!(json["decided_by"], "b");

    pane_signal(1, "working");
    let (code, json) = wait(&["--all", "--timeout", "300ms"]);
    assert_eq!(code, Some(1));
    assert_eq!(json["outcome"], "timed_out");
    assert_eq!(outcomes(&json), ["a=matched", "b=timed_out"]);

    // A helper that never signalled does not end an --all wait: it is
    // watched until its first signal, and is not_ready only at the timeout.
    std::fs::remove_file(sandbox.home.path().join("signals/agent1.json")).unwrap();
    std::thread::sleep(std::time::Duration::from_secs(5));
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(800));
            pane_signal(1, "done");
        });
        let (code, json) = wait(&["--all", "--timeout", "5s"]);
        assert_eq!(code, Some(0), "{json}");
        assert_eq!(outcomes(&json), ["a=matched", "b=matched"]);
    });
    std::fs::remove_file(sandbox.home.path().join("signals/agent1.json")).unwrap();
    let (code, json) = wait(&["--all", "--timeout", "500ms"]);
    assert_eq!(code, Some(3), "{json}");
    assert_eq!(json["outcome"], "not_ready");
    assert_eq!(outcomes(&json), ["a=matched", "b=not_ready"]);

    // Several targets need a mode; the two modes exclude each other.
    sandbox
        .pmux()
        .args([
            "wait",
            "a",
            "b",
            "--until",
            "agent:done",
            "--timeout",
            "1ms",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--all or --any"));
    sandbox
        .pmux()
        .args(["wait", "a", "b", "--all", "--any"])
        .assert()
        .code(2);
}
