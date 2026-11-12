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

/// A team machine: ~/.zshrc is a symlink into a shared dotfiles checkout that
/// sources ~/.zshrc.d/*, and already holds a pmux block from an earlier
/// `pmux init`. Unattended setup (no terminal, as from Ansible, with an
/// unexpanded `~`) puts the block in ~/.zshrc.d/pmux, takes it out of the
/// shared file, keeps using that file on later runs, and the uninstaller
/// removes it from there.
#[test]
fn rc_file_keeps_the_block_out_of_a_shared_zshrc() {
    let home = Home::new();
    let root = home.path();
    let dotfiles = root.join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    let shared = "export TEAM=1\nfor f in ~/.zshrc.d/*; do source $f; done\n";
    std::fs::write(
        dotfiles.join("zshrc"),
        format!("{shared}\n# >>> pmux wrap >>>\nold\n# <<< pmux wrap <<<\n"),
    )
    .unwrap();
    std::os::unix::fs::symlink("dotfiles/zshrc", root.join(".zshrc")).unwrap();
    let socket = format!("pmux-setup-rc-test-{}", std::process::id());
    let run = |program: &str, args: &[&str]| {
        let output = Command::new(program)
            .args(args)
            .env_clear()
            .env("HOME", root)
            .env("SHELL", "/bin/zsh")
            .env("PMUX_SOCKET", &socket)
            .env("PMUX_BIN_DIR", root.join("installed-bin"))
            .env("PMUX_SHARE_DIR", root.join("installed-share"))
            .env("PATH", format!("{}:/usr/bin:/bin", home.bin().display()))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let pmux = env!("CARGO_BIN_EXE_pmux");
    let args = [
        "setup",
        "--yes",
        "--skip",
        "ai-attn,dredge,wrap,tmux",
        "--rc-file",
        "~/.zshrc.d/pmux",
    ];

    let dry = run(pmux, &[&args[..], &["--dry-run"]].concat());
    assert!(dry.contains(".zshrc.d/pmux"), "{dry}");
    assert!(!root.join(".zshrc.d").exists());

    let stdout = run(pmux, &args);
    assert!(stdout.contains("Moved pmux's shell block"), "{stdout}");
    let block = home.read(".zshrc.d/pmux");
    assert!(block.contains("# >>> pmux wrap >>>"), "{block}");
    assert!(block.contains("pmux completions zsh"), "{block}");
    assert_eq!(home.read("dotfiles/zshrc"), shared);
    assert!(
        std::fs::symlink_metadata(root.join(".zshrc"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        home.read(".config/pmux/config.toml")
            .contains("[shell]\nrc_file = \"~/.zshrc.d/pmux\"")
    );
    assert_eq!(home.read("downloads"), "");

    let again = run(
        pmux,
        &["setup", "--yes", "--skip", "ai-attn,dredge,wrap,tmux"],
    );
    assert!(
        again.contains("Already set up in ~/.zshrc.d/pmux"),
        "{again}"
    );
    assert_eq!(home.read("dotfiles/zshrc"), shared);
    assert_eq!(home.read(".zshrc.d/pmux"), block);

    run(
        "sh",
        &[
            concat!(env!("CARGO_MANIFEST_DIR"), "/uninstall.sh"),
            "--force",
            "--keep-data",
        ],
    );
    assert!(!home.read(".zshrc.d/pmux").contains("pmux wrap"));
    assert_eq!(home.read("dotfiles/zshrc"), shared);
}
