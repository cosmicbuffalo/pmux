use assert_cmd::Command;
use predicates::str::contains;

#[test]
fn ambiguous_and_missing_local_times_are_rejected_before_target_resolution() {
    for (time, error) in [
        ("2027-11-07 01:30", "ambiguous"),
        ("2027-03-14 02:30", "skipped by daylight saving"),
        ("2027-02-30 12:00", "invalid date"),
    ] {
        Command::new(assert_cmd::cargo::cargo_bin!("pmux"))
            .env("TZ", "America/Chicago")
            .args(["queue", "schedule", "nonexistent", time])
            .assert()
            .failure()
            .stderr(contains(error));
    }
}

#[test]
fn queue_edit_requires_one_message_source() {
    Command::new(assert_cmd::cargo::cargo_bin!("pmux"))
        .args(["queue", "edit", "agent", "1", "--stdin", "ignored text"])
        .assert()
        .failure()
        .stderr(contains("cannot be used with"));
}
