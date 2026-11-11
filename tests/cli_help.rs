use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn default_invocation_shows_help_text() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
    cmd.assert()
        .success()
        .stdout(contains("process multiplexer"));
}

#[test]
fn help_subcommand_shows_help_text() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
    cmd.arg("help")
        .assert()
        .success()
        .stdout(contains("Every day:"));
}

#[test]
fn help_alias_works() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
    cmd.arg("h")
        .assert()
        .success()
        .stdout(contains("Shortcuts:"));
}

#[test]
fn version_flag_reports_package_version() {
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("pmux"));
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(contains(concat!("pmux ", env!("CARGO_PKG_VERSION"))));
}
