//! Best-effort propagation of a pmux session rename into the native session
//! store of the AI agent running inside it, so the agent's own resume picker
//! shows the pmux name.
//!
//! Each agent needs bespoke handling:
//! - Claude Code persists display names as append-only `custom-title` /
//!   `agent-name` records inside the session transcript at
//!   `$CLAUDE_CONFIG_DIR/projects/<encoded-cwd>/<session-id>.jsonl` (the same
//!   records `claude -n <name>` writes on launch).
//! - Codex maps session ids to display names in the append-only
//!   `$CODEX_HOME/session_index.jsonl`, where the newest record per id wins
//!   (the same file its `/rename` command updates).
//!
//! Both stores are internal formats, so failures here must never fail the
//! pmux rename itself — callers go through [`propagate_rename_best_effort`].

use crate::agents::{Agent, claude_config_dir, codex_home};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::util::now_epoch;

/// Push a completed pmux rename into the agent's own session store, warning
/// on stderr instead of failing: the tmux rename and state migration have
/// already committed, and the agent-side name is a courtesy.
pub(crate) fn propagate_rename_best_effort(new_session_name: &str) {
    match propagate_rename(new_session_name) {
        Ok(_) => {}
        Err(err) => eprintln!("pmux: warning: could not rename the agent's own session: {err:#}"),
    }
}

/// Rename the agent session recorded for a pmux session, returning the agent
/// kind that was renamed. `None` means there was nothing to do: the session
/// has no agent identity, the agent has no known rename mechanism, or only a
/// synthetic pane-scoped fallback id is known.
///
/// Must run after `migrate_session_state`, which moves the agent-identity
/// metadata file under the new session name.
pub(crate) fn propagate_rename(new_session_name: &str) -> Result<Option<&'static str>> {
    let Some(identity) = crate::state::read_session_agent_identity(new_session_name)? else {
        return Ok(None);
    };
    if !is_uuid(&identity.session_id) {
        return Ok(None);
    }
    match Agent::from_name(&identity.agent) {
        Some(Agent::Claude) => {
            rename_claude_session(
                &claude_config_dir()?,
                &identity.session_id,
                new_session_name,
            )?;
            Ok(Some(Agent::Claude.name()))
        }
        Some(Agent::Codex) => {
            let updated_at = rfc3339_utc(now_epoch());
            rename_codex_session(
                &codex_home()?,
                &identity.session_id,
                new_session_name,
                &updated_at,
            )?;
            Ok(Some(Agent::Codex.name()))
        }
        _ => Ok(None),
    }
}

/// Agent-native ids that pmux can rename are UUIDs. ai-attn also synthesizes
/// fallback ids like `codex-pane-3` when a hook cannot see the real session;
/// those identify a pane, not a stored session, so they are rejected here.
fn is_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

/// Append Claude Code's name records to every stored transcript for the
/// session id. Scanning the project directories avoids re-implementing
/// Claude's cwd-encoding scheme, and the id is unique so duplicate matches
/// are copies of the same session.
fn rename_claude_session(config_dir: &Path, session_id: &str, new_name: &str) -> Result<()> {
    let projects = config_dir.join("projects");
    let file_name = format!("{session_id}.jsonl");
    let title = serde_json::json!({
        "type": "custom-title",
        "customTitle": new_name,
        "sessionId": session_id,
    });
    let agent_name = serde_json::json!({
        "type": "agent-name",
        "agentName": new_name,
        "sessionId": session_id,
    });
    let records = format!("{title}\n{agent_name}\n");

    let entries = fs::read_dir(&projects)
        .with_context(|| format!("failed to read {}", projects.display()))?;
    let mut renamed = false;
    for entry in entries.flatten() {
        let transcript = entry.path().join(&file_name);
        if !transcript.is_file() {
            continue;
        }
        append_jsonl(&transcript, &records)?;
        renamed = true;
    }
    if !renamed {
        bail!(
            "no stored Claude Code session {} under {}",
            session_id,
            projects.display()
        );
    }
    Ok(())
}

/// Append a fresh `thread_name` record for the session id to codex's
/// session index; codex resolves the newest record per id.
fn rename_codex_session(
    codex_home: &Path,
    session_id: &str,
    new_name: &str,
    updated_at: &str,
) -> Result<()> {
    let index = codex_home.join("session_index.jsonl");
    if !index.is_file() {
        bail!("codex session index {} does not exist", index.display());
    }
    let record = serde_json::json!({
        "id": session_id,
        "thread_name": new_name,
        "updated_at": updated_at,
    });
    append_jsonl(&index, &format!("{record}\n"))
}

/// Append pre-terminated JSON lines to a live jsonl file, inserting a leading
/// newline if the owner's last write did not end with one.
fn append_jsonl(path: &Path, lines: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new()
        .read(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    let mut buf = String::new();
    if file.seek(SeekFrom::End(-1)).is_ok() {
        let mut last = [0u8; 1];
        if file.read_exact(&mut last).is_ok() && last[0] != b'\n' {
            buf.push('\n');
        }
    }
    buf.push_str(lines);
    file.write_all(buf.as_bytes())
        .with_context(|| format!("failed to append to {}", path.display()))
}

/// Format a unix timestamp as UTC RFC 3339 (`2026-09-10T18:51:46Z`), matching
/// the `updated_at` values codex writes to its session index.
fn rfc3339_utc(epoch_secs: i64) -> String {
    let days = epoch_secs.div_euclid(86_400);
    let secs = epoch_secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year,
        month,
        day,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Convert days since the unix epoch to a (year, month, day) civil date.
/// Howard Hinnant's `civil_from_days` algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_gate_accepts_real_ids_and_rejects_synthetic_ones() {
        assert!(is_uuid("d6732d29-b319-48e4-bc37-0e63d5b1bc13"));
        assert!(is_uuid("01a0461c-7006-7290-870e-a8902e03aeb8"));
        assert!(!is_uuid("codex-pane-3"));
        assert!(!is_uuid("codex-ppid-12345"));
        assert!(!is_uuid("pi-pane-7"));
        assert!(!is_uuid(""));
        assert!(!is_uuid("../../../../etc/passwd-0000-0000-0000"));
    }

    #[test]
    fn claude_rename_appends_title_records() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "d6732d29-b319-48e4-bc37-0e63d5b1bc13";
        let project = tmp.path().join("projects").join("-home-user-repo");
        fs::create_dir_all(&project).unwrap();
        let transcript = project.join(format!("{id}.jsonl"));
        // No trailing newline: a live writer may be mid-append.
        fs::write(&transcript, "{\"type\":\"session\"}").unwrap();

        rename_claude_session(tmp.path(), id, "my renamed \"session\"").unwrap();

        let text = fs::read_to_string(&transcript).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "expected original + two records: {text:?}");
        let title: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(title["type"], "custom-title");
        assert_eq!(title["customTitle"], "my renamed \"session\"");
        assert_eq!(title["sessionId"], id);
        let agent_name: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(agent_name["type"], "agent-name");
        assert_eq!(agent_name["agentName"], "my renamed \"session\"");
    }

    #[test]
    fn claude_rename_updates_every_matching_transcript() {
        let tmp = tempfile::tempdir().unwrap();
        let id = "d6732d29-b319-48e4-bc37-0e63d5b1bc13";
        for dir in ["-a", "-b"] {
            let project = tmp.path().join("projects").join(dir);
            fs::create_dir_all(&project).unwrap();
            fs::write(project.join(format!("{id}.jsonl")), "{}\n").unwrap();
        }

        rename_claude_session(tmp.path(), id, "new-name").unwrap();

        for dir in ["-a", "-b"] {
            let text = fs::read_to_string(
                tmp.path()
                    .join("projects")
                    .join(dir)
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
            assert!(
                text.contains("custom-title"),
                "missing record in {dir}: {text:?}"
            );
        }
    }

    #[test]
    fn claude_rename_fails_without_stored_session() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("projects")).unwrap();
        let err = rename_claude_session(
            tmp.path(),
            "d6732d29-b319-48e4-bc37-0e63d5b1bc13",
            "new-name",
        )
        .unwrap_err();
        assert!(err.to_string().contains("no stored Claude Code session"));
    }

    #[test]
    fn codex_rename_appends_index_record() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("session_index.jsonl");
        let id = "01a0461c-7006-7290-870e-a8902e03aeb8";
        fs::write(
            &index,
            format!("{{\"id\":\"{id}\",\"thread_name\":\"old\",\"updated_at\":\"2026-08-28T02:08:14.879678147Z\"}}\n"),
        )
        .unwrap();

        rename_codex_session(tmp.path(), id, "new-name", "2026-09-10T18:51:46Z").unwrap();

        let text = fs::read_to_string(&index).unwrap();
        let last: serde_json::Value = serde_json::from_str(text.lines().last().unwrap()).unwrap();
        assert_eq!(last["id"], id);
        assert_eq!(last["thread_name"], "new-name");
        assert_eq!(last["updated_at"], "2026-09-10T18:51:46Z");
    }

    #[test]
    fn codex_rename_fails_without_index() {
        let tmp = tempfile::tempdir().unwrap();
        let err = rename_codex_session(
            tmp.path(),
            "01a0461c-7006-7290-870e-a8902e03aeb8",
            "new-name",
            "2026-09-10T18:51:46Z",
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn rfc3339_utc_formats_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1_789_066_306), "2026-09-10T18:51:46Z");
        // Leap-year day.
        assert_eq!(rfc3339_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
