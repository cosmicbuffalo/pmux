//! Which process spawned which.
//!
//! A process started by `pmux run` or `pmux resume` from inside a pmux pane
//! records that pane's process as its parent, so clients can show spawned
//! processes under the process that started them. A parent may also be on
//! another machine, recorded as given by the tool that relayed the spawn.
//!
//! Records live in `parents/<child>` in the backend's state directory and are
//! written under the state lock. A record names its local parent by session
//! name *and* `session_created`: tmux reuses names, so a parent is reported
//! only while that same session is alive, and a process started later under
//! the parent's old name never adopts the orphans.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use crate::SessionInfo;

/// The process that started a pmux process, as reported in `ProcessInfo`.
///
/// For a local parent `machine` and `url` are null and `created_at` is the
/// parent session's `session_created`. For a remote parent `machine` and
/// `url` name the parent's machine and its base URL, and `created_at` may be null.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SpawnedBy {
    pub process: String,
    pub created_at: Option<i64>,
    pub machine: Option<String>,
    pub url: Option<String>,
}

impl SpawnedBy {
    pub(crate) fn local(process: &str, created_at: i64) -> Self {
        Self {
            process: process.to_string(),
            created_at: Some(created_at),
            machine: None,
            url: None,
        }
    }

    pub(crate) fn is_remote(&self) -> bool {
        self.machine.is_some()
    }

    /// "auth-refactor", or "auth-refactor on macbook" for a remote parent.
    pub(crate) fn describe(&self) -> String {
        match &self.machine {
            Some(machine) => format!("{} on {machine}", self.process),
            None => self.process.clone(),
        }
    }
}

/// How `pmux run` / `pmux resume` choose the new process's parent.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ParentSpec {
    /// The caller's pmux process when run inside a pmux pane, else none.
    #[default]
    Auto,
    /// Record no parent (`--no-parent`), e.g. a process a person started.
    None,
    /// An existing local process (`--parent NAME`).
    Local(String),
    /// A process on another machine (`--parent NAME --parent-machine M
    /// --parent-url U`), recorded as given.
    Remote {
        process: String,
        machine: String,
        url: String,
    },
}

/// What is stored per child: the parent plus the child's own creation epoch,
/// so a record left behind by an earlier session of the same name is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
struct ParentRecord {
    child_created_at: i64,
    parent: SpawnedBy,
}

/// Resolve a spec to the parent to record. Only an explicit parent can fail:
/// an automatic lookup that fails records nothing.
pub(crate) fn resolve(spec: &ParentSpec) -> Result<Option<SpawnedBy>> {
    match spec {
        ParentSpec::Auto => Ok(caller_parent()),
        ParentSpec::None => Ok(None),
        ParentSpec::Local(name) => {
            let created = crate::tmux::session_created_epoch(name)?
                .ok_or_else(|| anyhow!("parent process '{name}' is not running"))?;
            Ok(Some(SpawnedBy::local(name, created)))
        }
        ParentSpec::Remote {
            process,
            machine,
            url,
        } => {
            for (what, value) in [("parent", process), ("machine", machine), ("url", url)] {
                if value.trim().is_empty() {
                    bail!("remote {what} must not be empty");
                }
                if value.chars().any(char::is_control) {
                    bail!("remote {what} must not contain control characters");
                }
            }
            Ok(Some(SpawnedBy {
                process: process.clone(),
                created_at: None,
                machine: Some(machine.clone()),
                url: Some(url.clone()),
            }))
        }
    }
}

/// The name of the pmux process whose pane this command runs in, if any.
pub(crate) fn caller_process() -> Option<String> {
    caller_parent().map(|parent| parent.process)
}

/// The pmux process whose pane this command runs in, if any.
///
/// Pane ids are per server, so `$TMUX_PANE` is trusted only when `$TMUX`
/// names pmux's socket and tmux confirms the pane lives on that same socket.
fn caller_parent() -> Option<SpawnedBy> {
    if !crate::tmux::inside_pmux_client() {
        return None;
    }
    let pane = std::env::var("TMUX_PANE").ok()?;
    if !pane.starts_with('%') {
        return None;
    }
    let socket = crate::tmux::current_tmux_socket()?;
    let output = crate::tmux::tmux_cmd()
        .args([
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{socket_path}\t#{session_name}\t#{session_created}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_pane_lookup(&String::from_utf8_lossy(&output.stdout), &socket)
}

// Parse `socket_path\tsession_name\tsession_created`, accepting it only when
// the pane is on the caller's socket.
fn parse_pane_lookup(raw: &str, caller_socket: &str) -> Option<SpawnedBy> {
    let line = raw.trim_end_matches(['\n', '\r']);
    let (socket_path, rest) = line.split_once('\t')?;
    let (name, created) = rest.rsplit_once('\t')?;
    let created: i64 = created.parse().ok()?;
    if name.is_empty() || !same_socket(socket_path, caller_socket) {
        return None;
    }
    Some(SpawnedBy::local(name, created))
}

fn same_socket(a: &str, b: &str) -> bool {
    if a.is_empty() || b.is_empty() {
        return false;
    }
    a == b
        || matches!(
            (fs::canonicalize(a), fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// Record `parent` for `child`, whose tmux session was created at
/// `child_created_at`. The caller holds the state lock.
pub(crate) fn write_parent(child: &str, child_created_at: i64, parent: &SpawnedBy) -> Result<()> {
    let record = ParentRecord {
        child_created_at,
        parent: parent.clone(),
    };
    let json = serde_json::to_string(&record).context("failed to encode parent record")?;
    crate::state::write_state_file(
        &crate::state::pmux_parents_dir()?.join(child),
        &format!("{json}\n"),
    )
}

/// Remove `child`'s parent record, if any. The caller holds the state lock.
pub(crate) fn clear_parent(child: &str) -> Result<()> {
    let path = crate::state::pmux_parents_dir()?.join(child);
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", path.display())),
    }
}

fn read_record(path: &Path) -> Option<ParentRecord> {
    let raw = fs::read_to_string(path).ok()?;
    serde_json::from_str(raw.trim()).ok()
}

/// The parent recorded for the session that currently runs as `child`,
/// whether or not that parent is still alive.
pub(crate) fn recorded_parent(child: &str, child_created_at: i64) -> Option<SpawnedBy> {
    let dir = crate::state::pmux_parents_dir().ok()?;
    read_record(&dir.join(child))
        .filter(|record| record.child_created_at == child_created_at)
        .map(|record| record.parent)
}

/// The reportable parent of every session in `sessions`, keyed by name. A
/// local parent counts only while the same session (name and creation epoch)
/// is in `sessions`; a remote parent is reported as recorded.
pub(crate) fn live_parents(sessions: &[SessionInfo]) -> HashMap<String, SpawnedBy> {
    let alive: HashMap<&str, i64> = sessions
        .iter()
        .map(|session| (session.name.as_str(), session.created_epoch))
        .collect();
    sessions
        .iter()
        .filter_map(|session| {
            let parent = recorded_parent(&session.name, session.created_epoch)?;
            is_reportable(&parent, &alive).then(|| (session.name.clone(), parent))
        })
        .collect()
}

fn is_reportable(parent: &SpawnedBy, alive: &HashMap<&str, i64>) -> bool {
    parent.is_remote()
        || (parent.created_at.is_some()
            && alive.get(parent.process.as_str()).copied() == parent.created_at)
}

/// After the parent session created at `created_at` was renamed from `old`
/// to `new`, point its children's records at the new name. The caller holds
/// the state lock.
pub(crate) fn rename_parent_references(old: &str, new: &str, created_at: i64) -> Result<()> {
    let dir = crate::state::pmux_parents_dir()?;
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", dir.display())),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(mut record) = read_record(&path) else {
            continue;
        };
        if record.parent.is_remote()
            || record.parent.process != old
            || record.parent.created_at != Some(created_at)
        {
            continue;
        }
        record.parent.process = new.to_string();
        let json = serde_json::to_string(&record).context("failed to encode parent record")?;
        crate::state::write_state_file(&path, &format!("{json}\n"))?;
    }
    Ok(())
}

/// The processes `root` spawned: its direct children, or with `all_levels`
/// every process below it. Names come in `sessions` order.
pub(crate) fn spawned_under(root: &str, sessions: &[SessionInfo], all_levels: bool) -> Vec<String> {
    let parents = live_parents(sessions);
    let local_parent = |name: &str| {
        parents
            .get(name)
            .filter(|parent| !parent.is_remote())
            .map(|parent| parent.process.as_str())
    };
    sessions
        .iter()
        .map(|session| session.name.as_str())
        .filter(|name| {
            let mut current = local_parent(name);
            // Records are cycle-checked when written, but stop at the
            // session count in case one was edited by hand.
            for _ in 0..sessions.len() {
                match current {
                    Some(parent) if parent == root => return true,
                    Some(parent) if all_levels => current = local_parent(parent),
                    _ => return false,
                }
            }
            false
        })
        .map(str::to_string)
        .collect()
}

/// Refuse a local parent that would make `child` its own ancestor.
pub(crate) fn check_no_cycle(
    child: &str,
    parent: &SpawnedBy,
    sessions: &[SessionInfo],
) -> Result<()> {
    if parent.is_remote() {
        return Ok(());
    }
    let parents = live_parents(sessions);
    let mut current = Some(parent.process.clone());
    let mut steps = 0;
    while let Some(name) = current {
        if name == child {
            bail!("'{}' cannot be the parent of its own ancestor", child);
        }
        steps += 1;
        if steps > sessions.len() {
            break;
        }
        current = parents
            .get(&name)
            .filter(|p| !p.is_remote())
            .map(|p| p.process.clone());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::with_temp_home;

    fn session(name: &str, created: i64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pane_id: format!("%{created}"),
            created_epoch: created,
            last_activity_epoch: created,
            current_command: "sh".to_string(),
            start_command: None,
            current_path: None,
            launch_command: None,
            launch_cwd: None,
            attached_clients: 0,
        }
    }

    fn remote() -> SpawnedBy {
        SpawnedBy {
            process: "lineage ios".to_string(),
            created_at: None,
            machine: Some("macbook".to_string()),
            url: Some("http://100.72.15.45:9470".to_string()),
        }
    }

    #[test]
    fn pane_lookup_requires_the_callers_socket() {
        let raw = "/tmp/tmux-1000/pmux\tauth refactor\t1793470800\n";
        assert_eq!(
            parse_pane_lookup(raw, "/tmp/tmux-1000/pmux"),
            Some(SpawnedBy::local("auth refactor", 1793470800))
        );
        assert_eq!(parse_pane_lookup(raw, "/tmp/tmux-1000/default"), None);
        assert_eq!(parse_pane_lookup("", "/tmp/tmux-1000/pmux"), None);
        assert_eq!(
            parse_pane_lookup("/tmp/tmux-1000/pmux\t\t17\n", "/tmp/tmux-1000/pmux"),
            None
        );
    }

    #[test]
    fn spawned_by_serializes_nulls_for_a_local_parent() {
        let json = serde_json::to_value(SpawnedBy::local("auth-refactor", 1793470800)).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "process": "auth-refactor",
                "created_at": 1793470800,
                "machine": null,
                "url": null,
            })
        );
        assert_eq!(remote().describe(), "lineage ios on macbook");
    }

    #[test]
    fn a_local_parent_is_reported_only_while_the_same_session_lives() {
        with_temp_home(|_| {
            write_parent("child", 20, &SpawnedBy::local("parent", 10)).unwrap();
            write_parent("orphan", 21, &SpawnedBy::local("gone", 11)).unwrap();
            write_parent("adopted", 22, &SpawnedBy::local("reused", 12)).unwrap();
            write_parent("far", 23, &remote()).unwrap();
            // A record from an earlier session named "stale".
            write_parent("stale", 1, &SpawnedBy::local("parent", 10)).unwrap();
            let sessions = vec![
                session("parent", 10),
                session("child", 20),
                session("orphan", 21),
                session("adopted", 22),
                session("far", 23),
                session("stale", 24),
                // Same name as the recorded parent, but a later session.
                session("reused", 99),
            ];
            let parents = live_parents(&sessions);
            assert_eq!(parents.get("child"), Some(&SpawnedBy::local("parent", 10)));
            assert_eq!(parents.get("far"), Some(&remote()));
            assert!(!parents.contains_key("orphan"), "dead parent is omitted");
            assert!(!parents.contains_key("adopted"), "reused name is rejected");
            assert!(!parents.contains_key("stale"), "old child record ignored");
        });
    }

    #[test]
    fn renaming_a_parent_rewrites_only_its_own_children() {
        with_temp_home(|_| {
            write_parent("a", 20, &SpawnedBy::local("p", 10)).unwrap();
            write_parent("b", 21, &SpawnedBy::local("p", 5)).unwrap();
            let mut far = remote();
            far.process = "p".to_string();
            write_parent("c", 22, &far).unwrap();
            rename_parent_references("p", "q", 10).unwrap();
            assert_eq!(recorded_parent("a", 20), Some(SpawnedBy::local("q", 10)));
            assert_eq!(recorded_parent("b", 21), Some(SpawnedBy::local("p", 5)));
            assert_eq!(recorded_parent("c", 22), Some(far));
        });
    }

    #[test]
    fn cycles_are_refused() {
        with_temp_home(|_| {
            let sessions = vec![session("a", 1), session("b", 2), session("c", 3)];
            write_parent("b", 2, &SpawnedBy::local("a", 1)).unwrap();
            write_parent("c", 3, &SpawnedBy::local("b", 2)).unwrap();
            assert!(check_no_cycle("a", &SpawnedBy::local("c", 3), &sessions).is_err());
            assert!(check_no_cycle("a", &SpawnedBy::local("a", 1), &sessions).is_err());
            assert!(check_no_cycle("c", &SpawnedBy::local("a", 1), &sessions).is_ok());
            assert!(check_no_cycle("a", &remote(), &sessions).is_ok());
        });
    }

    #[test]
    fn spawned_under_finds_children_or_every_descendant() {
        with_temp_home(|_| {
            let sessions = vec![
                session("root", 1),
                session("a", 2),
                session("b", 3),
                session("a1", 4),
                session("other", 5),
                session("far", 6),
            ];
            write_parent("a", 2, &SpawnedBy::local("root", 1)).unwrap();
            write_parent("b", 3, &SpawnedBy::local("root", 1)).unwrap();
            write_parent("a1", 4, &SpawnedBy::local("a", 2)).unwrap();
            let mut far = remote();
            far.process = "root".to_string();
            write_parent("far", 6, &far).unwrap();
            assert_eq!(spawned_under("root", &sessions, false), ["a", "b"]);
            assert_eq!(spawned_under("root", &sessions, true), ["a", "b", "a1"]);
            assert_eq!(spawned_under("a", &sessions, true), ["a1"]);
            assert!(spawned_under("other", &sessions, true).is_empty());
        });
    }

    #[test]
    fn remote_specs_are_validated_and_recorded_as_given() {
        let spec = ParentSpec::Remote {
            process: "lineage ios".into(),
            machine: "macbook".into(),
            url: "http://100.72.15.45:9470".into(),
        };
        assert_eq!(resolve(&spec).unwrap(), Some(remote()));
        let bad = ParentSpec::Remote {
            process: "x".into(),
            machine: " ".into(),
            url: "http://h".into(),
        };
        assert!(resolve(&bad).is_err());
        assert_eq!(resolve(&ParentSpec::None).unwrap(), None);
    }
}
