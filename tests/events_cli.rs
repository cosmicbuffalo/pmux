mod common;

use std::process::Command;
use std::time::{Duration, Instant};

// Regression: `events` only writes when state changes, so a follower
// whose reader had died ran forever (orphaned followers from test runs kept
// recreating their deleted test HOMEs). It now exits once its parent is gone.
#[test]
fn follow_exits_when_its_parent_exits() {
    let home = tempfile::tempdir().unwrap();
    let pmux = assert_cmd::cargo::cargo_bin!("pmux");
    let output = Command::new("sh")
        .arg("-c")
        // Stay alive briefly so the follower records this shell as its parent.
        .arg(r#""$0" events --interval 50ms >/dev/null 2>&1 & echo $!; sleep 0.5"#)
        .arg(pmux)
        .env("HOME", home.path())
        .env("PATH", common::path())
        .env(
            "PMUX_SOCKET",
            format!("pmux-events-test-{}", std::process::id()),
        )
        .env_remove("TMUX")
        .output()
        .unwrap();
    let pid: i32 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let alive = |pid: i32| {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    };
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let still_alive = alive(pid);
    if still_alive {
        let _ = Command::new("kill").arg(pid.to_string()).status();
    }
    assert!(!still_alive, "orphaned `events` did not exit");
}
