//! `pmux setup` in a scratch home with stub companions; never uses the network.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn executable(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    /// A home whose PATH holds current ai-attn and Dredge stubs, a `codex`
    /// agent, and downloaders that fail, so no step can reach the network.
    fn new() -> Self {
        let home = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        let bin = home.bin();
        executable(
            &bin.join("ai-attn"),
            "#!/bin/sh\ncase \"$1\" in\n  version) echo 'ai-attn v9.0.0' ;;\n  setup) echo \"setup $*\" >> \"$HOME/ai-attn.log\" ;;\nesac\n",
        );
        executable(&bin.join("dredge"), "#!/bin/sh\necho 'dredge 9.0.0'\n");
        executable(&bin.join("codex"), "#!/bin/sh\n");
        for downloader in ["curl", "wget"] {
            executable(
                &bin.join(downloader),
                "#!/bin/sh\necho download >> \"$HOME/downloads\"\nexit 1\n",
            );
        }
        std::fs::write(home.path().join(".tmux.conf"), "set -g mouse on\n").unwrap();
        home
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn bin(&self) -> std::path::PathBuf {
        self.path().join("bin")
    }

    fn setup(&self, args: &[&str]) -> Output {
        let output = Command::new(env!("CARGO_BIN_EXE_pmux"))
            .arg("setup")
            .args(args)
            .env_clear()
            .env("HOME", self.path())
            .env("SHELL", "/bin/bash")
            .env(
                "PMUX_SOCKET",
                format!("pmux-setup-test-{}", std::process::id()),
            )
            .env("PATH", format!("{}:/usr/bin:/bin", self.bin().display()))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.path().join(name)).unwrap_or_default()
    }
}

#[test]
fn dry_run_changes_nothing() {
    let home = Home::new();
    let stdout = String::from_utf8(home.setup(&["--dry-run"]).stdout).unwrap();
    assert!(stdout.contains("Would wrap codex."), "{stdout}");
    assert!(stdout.contains("Would add `source-file"), "{stdout}");
    assert!(stdout.contains("Would run `ai-attn setup`"), "{stdout}");
    assert_eq!(home.read(".tmux.conf"), "set -g mouse on\n");
    assert!(!home.path().join(".bashrc").exists());
    assert!(!home.path().join(".config").exists());
    assert_eq!(home.read("downloads"), "");
}

#[test]
fn yes_wires_everything_and_a_rerun_changes_nothing() {
    let home = Home::new();
    let stdout = String::from_utf8(home.setup(&["--yes"]).stdout).unwrap();
    assert!(stdout.contains("Wrapped codex."), "{stdout}");
    assert_eq!(home.read("ai-attn.log"), "setup setup\n");
    assert!(home.read(".bashrc").contains("pmux"));
    let conf = home.read(".tmux.conf");
    assert_eq!(conf.matches("pmux/outer-tmux.conf").count(), 1, "{conf}");
    assert!(conf.starts_with("set -g mouse on\n"));

    let again = String::from_utf8(home.setup(&["--yes"]).stdout).unwrap();
    assert!(again.contains("Already wrapped: codex."), "{again}");
    assert!(
        again.contains("already loads pmux's integration"),
        "{again}"
    );
    assert_eq!(home.read(".tmux.conf"), conf);
    assert_eq!(home.read("downloads"), "");
}

#[test]
fn without_a_terminal_it_asks_for_nothing_and_says_what_is_left() {
    let home = Home::new();
    let stdout = String::from_utf8(home.setup(&["--skip", "ai-attn,dredge"]).stdout).unwrap();
    assert!(stdout.contains("Wrap codex: run `pmux setup`"), "{stdout}");
    assert!(
        stdout.contains("Connect pmux to your tmux (~/.tmux.conf)"),
        "{stdout}"
    );
    assert_eq!(home.read(".tmux.conf"), "set -g mouse on\n");
    assert_eq!(home.read("ai-attn.log"), "");
}
