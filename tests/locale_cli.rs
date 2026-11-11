mod common;

use assert_cmd::Command;

// Without a UTF-8 locale tmux writes tabs in `-F` output as `_`, so pmux used
// to parse no sessions at all and report "No running pmux processes" from
// boot services, cron, or any environment without LANG.
#[test]
fn listing_works_without_a_locale() {
    if std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .is_err()
    {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let tmux_tmp = tempfile::Builder::new()
        .prefix("pmux-locale-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = format!("pmux-locale-test-{}", std::process::id());
    let path = common::path();
    // A bare environment: no LANG, LC_ALL, LC_CTYPE, or TERM.
    let pmux = || {
        let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
        cmd.env_clear()
            .env("HOME", home.path())
            .env("PATH", &path)
            .env("TMUX_TMPDIR", tmux_tmp.path())
            .env("PMUX_SOCKET", &socket);
        cmd
    };

    pmux()
        .args(["run", "-d", "-s", "probe", "sleep", "60"])
        .assert()
        .success();
    let listed = pmux().args(["ls", "--json"]).output().unwrap();

    let _ = std::process::Command::new("tmux")
        .env("TMUX_TMPDIR", tmux_tmp.path())
        .args(["-L", &socket, "kill-server"])
        .status();
    let json: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let names: Vec<_> = json["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|session| session["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, ["probe"], "{json}");
}
