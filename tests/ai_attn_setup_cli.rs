//! Loader decisions use real, private tmux servers and fake plugins; no network.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

const SETUP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/templates/ai-attn-setup.sh");

struct Server {
    home: tempfile::TempDir,
    sockets: tempfile::TempDir,
    name: String,
    context: String,
}

fn executable(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

impl Server {
    fn new(tag: &str) -> Option<Self> {
        if Command::new("tmux").arg("-V").output().is_err() {
            return None;
        }
        let mut server = Self {
            home: tempfile::tempdir().unwrap(),
            sockets: tempfile::Builder::new()
                .prefix("pmux-loader-")
                .tempdir_in("/tmp")
                .unwrap(),
            name: format!("pmux-loader-{tag}-{}", std::process::id()),
            context: String::new(),
        };
        // A missing plugin must never cause these tests to use the network.
        for binary in ["curl", "wget"] {
            executable(
                &server.home.path().join("bin").join(binary),
                "#!/bin/sh\nprintf 'download\\n' >> \"$HOME/downloads\"\nexit 1\n",
            );
        }
        success(
            server
                .tmux()
                .args(["new-session", "-d", "-s", "probe", "sleep", "60"])
                .output()
                .unwrap(),
        );
        server.context = String::from_utf8(
            success(
                server
                    .tmux()
                    .args(["display-message", "-p", "#{socket_path},#{pid},0"])
                    .output()
                    .unwrap(),
            )
            .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        success(
            server
                .tmux()
                .args([
                    "set-hook",
                    "-g",
                    "client-attached[100]",
                    "display-message unrelated",
                ])
                .output()
                .unwrap(),
        );
        Some(server)
    }

    fn env(&self, cmd: &mut Command) {
        cmd.env("HOME", self.home.path())
            .env("XDG_STATE_HOME", self.home.path().join("state"))
            .env("TMUX_TMPDIR", self.sockets.path())
            .env_remove("TMUX_PANE")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.home.path().join("bin").display(),
                    common::path()
                ),
            );
    }

    fn tmux(&self) -> Command {
        let mut cmd = Command::new("tmux");
        self.env(&mut cmd);
        cmd.env_remove("TMUX")
            .args(["-L", &self.name, "-f", "/dev/null"]);
        cmd
    }

    fn shell(&self, script: &Path) -> Command {
        let mut cmd = Command::new("sh");
        self.env(&mut cmd);
        cmd.env("TMUX", &self.context).arg(script);
        cmd
    }

    fn setup(&self, plugin: &Path) -> Child {
        self.shell(Path::new(SETUP))
            .env("PMUX_AI_ATTN_PLUGIN_DIR", plugin)
            .spawn()
            .unwrap()
    }

    fn plugin(&self, name: &str) -> PathBuf {
        let dir = self.home.path().join("plugins").join(name);
        executable(&dir.join("scripts/start-loop.sh"), "#!/bin/sh\nexit 0\n");
        executable(
            &dir.join("tmux-ai-attn.tmux"),
            r#"#!/bin/sh
# Simulate the old append-style plugin and a slow initialization.
printf '%s\n' "$0" >> "$HOME/loads"
sleep 1
dir=$(dirname "$0")
tmux set-hook -ag client-attached "run-shell -b '$dir/scripts/start-loop.sh'"
"#,
        );
        dir
    }

    fn assert_one_copy(&self, dir: &Path) {
        let loads = std::fs::read_to_string(self.home.path().join("loads")).unwrap();
        assert_eq!(
            loads.lines().collect::<Vec<_>>(),
            [dir.join("tmux-ai-attn.tmux").to_str().unwrap()]
        );
        let hooks = String::from_utf8(
            success(self.tmux().args(["show-hooks", "-g"]).output().unwrap()).stdout,
        )
        .unwrap();
        assert_eq!(
            hooks
                .lines()
                .filter(|line| line.contains("start-loop.sh"))
                .count(),
            1,
            "{hooks}"
        );
        assert!(hooks.contains(dir.to_str().unwrap()), "{hooks}");
        assert!(hooks.contains("display-message unrelated"), "{hooks}");
        let owner = success(
            self.tmux()
                .args(["show-option", "-gqv", "@pmux_ai_attn_loaded"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            String::from_utf8(owner.stdout).unwrap().trim(),
            dir.to_str().unwrap()
        );
        assert!(!self.home.path().join("downloads").exists());
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tmux().arg("kill-server").output();
    }
}

#[test]
fn delayed_tpm_hooks_win_over_two_background_setups() {
    for same_path in [false, true] {
        let Some(server) = Server::new(if same_path { "tpm-same" } else { "tpm" }) else {
            return;
        };
        let pmux_copy = server.plugin("pmux");
        let tpm_copy = if same_path {
            pmux_copy.clone()
        } else {
            server.plugin("tpm")
        };
        // Reproduce the actual config ordering: background setup helpers,
        // followed by TPM's synchronous run-shell in the same source-file.
        let mut config = String::new();
        for index in 0..2 {
            let wrapper = server.home.path().join(format!("setup-{index}.sh"));
            let done = server.home.path().join(format!("done-{index}"));
            executable(
                &wrapper,
                &format!(
                    "#!/bin/sh\nPMUX_AI_ATTN_PLUGIN_DIR='{}' sh '{}'\nprintf done > '{}'\n",
                    pmux_copy.display(),
                    SETUP,
                    done.display()
                ),
            );
            config.push_str(&format!("run-shell -b 'sh \"{}\"'\n", wrapper.display()));
        }
        config.push_str(&format!(
            "run-shell 'sleep 0.2; sh \"{}\"'\n",
            tpm_copy.join("tmux-ai-attn.tmux").display()
        ));
        let file = server.home.path().join("loader.conf");
        std::fs::write(&file, config).unwrap();
        success(server.tmux().arg("source-file").arg(file).output().unwrap());
        let started = Instant::now();
        while !(0..2).all(|index| server.home.path().join(format!("done-{index}")).exists()) {
            assert!(
                started.elapsed() < Duration::from_secs(8),
                "background setups did not finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        server.assert_one_copy(&tpm_copy);
    }
}

#[test]
fn concurrent_same_and_different_pmux_copies_make_one_server_decision() {
    for same in [true, false] {
        let Some(server) = Server::new(if same { "same" } else { "different" }) else {
            return;
        };
        let first_copy = server.plugin("first");
        let second_copy = if same {
            first_copy.clone()
        } else {
            server.plugin("second")
        };
        let first = server.setup(&first_copy);
        let second = server.setup(&second_copy);
        success(first.wait_with_output().unwrap());
        success(second.wait_with_output().unwrap());
        let loads = std::fs::read_to_string(server.home.path().join("loads")).unwrap();
        let winner = Path::new(loads.trim()).parent().unwrap();
        server.assert_one_copy(winner);
        // Reloads must not invoke even the same plugin again.
        success(server.setup(winner).wait_with_output().unwrap());
        server.assert_one_copy(winner);
    }
}

#[test]
fn unavailable_plugin_releases_locks_and_never_executes_unverified_code() {
    let Some(server) = Server::new("absent") else {
        return;
    };
    let plugin = server.home.path().join("plugins/missing");
    executable(
        &server.home.path().join("bin/curl"),
        r#"#!/bin/sh
if ! mkdir "$HOME/download-active"; then
  printf 'overlap\n' >> "$HOME/download-overlap"
  exit 1
fi
printf 'download\n' >> "$HOME/downloads"
sleep 1
rmdir "$HOME/download-active"
exit 1
"#,
    );
    let first = server.setup(&plugin);
    let second = server.setup(&plugin);
    success(first.wait_with_output().unwrap());
    success(second.wait_with_output().unwrap());
    assert!(!plugin.exists());
    assert!(!server.home.path().join("loads").exists());
    assert!(
        !server
            .home
            .path()
            .join("plugins/.tmux-ai-attn.lock")
            .exists()
    );
    assert!(!server.home.path().join("download-overlap").exists());
    assert_eq!(
        std::fs::read_to_string(server.home.path().join("downloads"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    // A successful download with the wrong digest must still never be unpacked/run.
    executable(
        &server.home.path().join("bin/curl"),
        "#!/bin/sh\nwhile [ \"$1\" != -o ]; do shift; done\nprintf 'untrusted archive' > \"$2\"\n",
    );
    success(server.setup(&plugin).wait_with_output().unwrap());
    assert!(!plugin.exists());
    let log =
        std::fs::read_to_string(server.home.path().join("state/tmux-ai-attn/hooks.log")).unwrap();
    assert!(log.contains("archive SHA-256"), "{log}");
}

#[test]
fn lock_timeout_fails_closed_without_removing_another_owners_lock() {
    let Some(server) = Server::new("locks") else {
        return;
    };
    let plugin = server.plugin("first");
    // Speed only the lock-timeout fixture up; all contention/grace tests use real sleep.
    executable(&server.home.path().join("bin/sleep"), "#!/bin/sh\nexit 0\n");
    let mut context = server.context.split(',');
    let socket = context.next().unwrap();
    let pid = context.next().unwrap();
    for lock in [
        PathBuf::from(format!("{socket}.pmux-ai-attn-{pid}.lock")),
        plugin.parent().unwrap().join(".tmux-ai-attn.lock"),
    ] {
        std::fs::create_dir(&lock).unwrap();
        let started = Instant::now();
        success(server.setup(&plugin).wait_with_output().unwrap());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(lock.exists());
        assert!(!server.home.path().join("loads").exists());
        std::fs::remove_dir(&lock).unwrap();
    }
}

#[test]
fn shared_plugin_can_load_once_in_each_of_two_servers() {
    let (Some(first), Some(second)) = (Server::new("server-one"), Server::new("server-two")) else {
        return;
    };
    let plugin = first.plugin("shared");
    let first_setup = first.setup(&plugin);
    let second_setup = second
        .shell(Path::new(SETUP))
        .env("HOME", first.home.path())
        .env("PMUX_AI_ATTN_PLUGIN_DIR", &plugin)
        .spawn()
        .unwrap();
    success(first_setup.wait_with_output().unwrap());
    success(second_setup.wait_with_output().unwrap());
    assert_eq!(
        std::fs::read_to_string(first.home.path().join("loads"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    for server in [&first, &second] {
        let hooks = String::from_utf8(
            success(server.tmux().args(["show-hooks", "-g"]).output().unwrap()).stdout,
        )
        .unwrap();
        assert_eq!(
            hooks
                .lines()
                .filter(|line| line.contains("start-loop.sh"))
                .count(),
            1
        );
        let owner = success(
            server
                .tmux()
                .args(["show-option", "-gqv", "@pmux_ai_attn_loaded"])
                .output()
                .unwrap(),
        );
        assert_eq!(
            String::from_utf8(owner.stdout).unwrap().trim(),
            plugin.to_str().unwrap()
        );
    }
}

/// A server that loaded pmux's managed copy before pmux was upgraded gets the
/// newly pinned release: the copy is replaced, its entry runs once, and a
/// later setup on the same server leaves it alone.
#[test]
fn loaded_managed_copy_is_upgraded_to_a_new_pin_once() {
    let Some(server) = Server::new("upgrade") else {
        return;
    };
    let home = server.home.path();
    let managed = home.join(".config/pmux/plugins/tmux-ai-attn");

    // The copy an earlier pmux installed and this server loaded.
    executable(
        &managed.join("scripts/start-loop.sh"),
        "#!/bin/sh\nexit 0\n",
    );
    executable(&managed.join("tmux-ai-attn.tmux"), "#!/bin/sh\nexit 1\n");
    std::fs::write(managed.join(".pmux-commit"), "old\n").unwrap();
    let old_hook = format!("run-shell -b '{}/scripts/start-loop.sh'", managed.display());
    success(
        server
            .tmux()
            .args(["set-hook", "-g", "client-attached[0]", &old_hook])
            .output()
            .unwrap(),
    );
    success(
        server
            .tmux()
            .args([
                "set-option",
                "-g",
                "@pmux_ai_attn_loaded",
                managed.to_str().unwrap(),
            ])
            .output()
            .unwrap(),
    );

    // The release the new pin names, served by a fake curl as GitHub's archive.
    let commit = "1111111111111111111111111111111111111111";
    let source = home.join("release").join(format!("tmux-ai-attn-{commit}"));
    executable(&source.join("scripts/start-loop.sh"), "#!/bin/sh\nexit 0\n");
    executable(
        &source.join("tmux-ai-attn.tmux"),
        r#"#!/bin/sh
printf 'new\n' >> "$HOME/loads"
dir=$(cd "$(dirname "$0")" && pwd)
tmux set-hook -g 'client-attached[50]' "run-shell -b '$dir/scripts/start-loop.sh'"
"#,
    );
    let archive = home.join("release.tar.gz");
    success(
        Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(home.join("release"))
            .arg(format!("tmux-ai-attn-{commit}"))
            .output()
            .unwrap(),
    );
    let sha = String::from_utf8(
        success(Command::new("sha256sum").arg(&archive).output().unwrap()).stdout,
    )
    .unwrap()
    .split_whitespace()
    .next()
    .unwrap()
    .to_string();
    executable(
        &home.join("bin/curl"),
        &format!(
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do [ \"$1\" = -o ] && out=$2; shift; done\nprintf 'download\\n' >> \"$HOME/downloads\"\ncp '{}' \"$out\"\n",
            archive.display()
        ),
    );

    // This pmux's setup script, pinned to that release.
    let script = std::fs::read_to_string(SETUP).unwrap();
    let pinned = script
        .lines()
        .map(|line| {
            if line.starts_with("REQUIRED_PLUGIN_COMMIT=") {
                format!("REQUIRED_PLUGIN_COMMIT=\"{commit}\"")
            } else if line.starts_with("REQUIRED_PLUGIN_SHA256=") {
                format!("REQUIRED_PLUGIN_SHA256=\"{sha}\"")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let setup = home.join("ai-attn-setup.sh");
    executable(&setup, &pinned);

    for _ in 0..2 {
        success(server.shell(&setup).output().unwrap());
    }

    assert_eq!(
        std::fs::read_to_string(managed.join(".pmux-commit"))
            .unwrap()
            .trim(),
        commit
    );
    assert_eq!(
        std::fs::read_to_string(home.join("loads")).unwrap(),
        "new\n"
    );
    assert_eq!(
        std::fs::read_to_string(home.join("downloads"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let hooks = String::from_utf8(
        success(server.tmux().args(["show-hooks", "-g"]).output().unwrap()).stdout,
    )
    .unwrap();
    let loops: Vec<_> = hooks
        .lines()
        .filter(|l| l.contains("start-loop.sh"))
        .collect();
    assert_eq!(loops.len(), 1, "{hooks}");
    assert!(loops[0].starts_with("client-attached[50]"), "{hooks}");
    assert!(hooks.contains("display-message unrelated"), "{hooks}");
    let owner = success(
        server
            .tmux()
            .args(["show-option", "-gqv", "@pmux_ai_attn_loaded"])
            .output()
            .unwrap(),
    );
    assert_eq!(
        String::from_utf8(owner.stdout).unwrap().trim(),
        managed.to_str().unwrap()
    );
}
