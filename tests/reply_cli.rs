//! Reply extraction uses native transcripts across isolated agent respawns.

mod common;

use assert_cmd::Command;

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
            .env("CODEX_HOME", self.home.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.home.path().join("claude"))
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
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

#[test]
fn reply_reads_existing_transcripts_after_codex_and_claude_respawn() {
    let Some(sandbox) = Sandbox::new("reply") else {
        return;
    };
    let bin = sandbox.home.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    for agent in ["codex", "claude"] {
        let fake = bin.join(agent);
        std::fs::write(&fake, "#!/bin/sh\nexec sleep 60\n").unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        sandbox
            .pmux()
            .args(["run", "-d", "-s", agent, agent])
            .assert()
            .success();
        let transcript = if agent == "codex" {
            sandbox
                .home
                .path()
                .join("codex/sessions/2020/01/01/rollout-old-native-id.jsonl")
        } else {
            sandbox
                .home
                .path()
                .join("claude/projects/test/native-id.jsonl")
        };
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        let record = if agent == "codex" {
            serde_json::json!({"type":"response_item", "timestamp":"2020-01-01T00:00:00Z", "payload":{"type":"message", "role":"assistant", "channel":"final", "content":[{"type":"output_text", "text":"Reply from before respawn.\nSecond line."}]}})
        } else {
            serde_json::json!({"type":"assistant", "message":{"role":"assistant", "content":[{"type":"text", "text":"Reply from before respawn.\nSecond line."}]}})
        };
        std::fs::write(&transcript, format!("{record}\n")).unwrap();
        sandbox
            .pmux()
            .args([
                "respawn",
                agent,
                "--resume-id",
                "native-id",
                "--force",
                "--yes",
            ])
            .assert()
            .success();
        let out = sandbox
            .pmux()
            .args(["reply", agent, "--json"])
            .assert()
            .success()
            .get_output()
            .clone();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(json["available"], true, "{json}");
        assert_eq!(json["session_id"], "native-id");
        assert_eq!(json["text"], "Reply from before respawn.\nSecond line.");
        sandbox
            .pmux()
            .args(["reply", agent])
            .assert()
            .success()
            .stdout("Reply from before respawn.\nSecond line.\n");
        std::fs::write(&transcript, "").unwrap();
        sandbox
            .pmux()
            .args(["reply", agent, "--json"])
            .assert()
            .success()
            .stdout(predicates::str::contains("\"available\": false"));
    }
    sandbox.pmux().args(["reply", "missing"]).assert().failure();
}
