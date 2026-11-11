//! Persistent sidebar grouping preferences.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use crate::state::ensure_pmux_state_dir;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AutomaticGrouping {
    #[default]
    Ungrouped,
    Window,
    Cwd,
    Agent,
}

impl AutomaticGrouping {
    pub fn next(self) -> Self {
        match self {
            Self::Ungrouped => Self::Window,
            Self::Window => Self::Cwd,
            Self::Cwd => Self::Agent,
            Self::Agent => Self::Ungrouped,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ungrouped => "ungrouped",
            Self::Window => "window",
            Self::Cwd => "CWD",
            Self::Agent => "agent",
        }
    }

    pub fn state_key(self) -> &'static str {
        match self {
            Self::Ungrouped => "ungrouped",
            Self::Window => "window",
            Self::Cwd => "cwd",
            Self::Agent => "agent",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct ManualGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SidebarGroupingState {
    #[serde(default)]
    pub automatic: AutomaticGrouping,
    #[serde(default)]
    pub manual: Vec<ManualGroup>,
    /// Per-mode order of automatic group keys. Newly discovered keys append.
    #[serde(default)]
    pub automatic_order: HashMap<String, Vec<String>>,
    /// Modes whose automatic order the user explicitly changed. Without this
    /// marker, `automatic_order` is only a cache of the live default order.
    #[serde(default)]
    pub automatic_custom_order: HashSet<String>,
    /// Stable row identities (`auto:<mode>:<key>` / `manual:<id>`).
    #[serde(default)]
    pub collapsed: HashSet<String>,
    /// Whether the Resumable group of past agent sessions is unfolded. Unlike
    /// the groups in `collapsed`, it starts folded.
    #[serde(default)]
    pub resumable_expanded: bool,
}

fn state_path() -> Result<std::path::PathBuf> {
    Ok(ensure_pmux_state_dir()?.join("sidebar-groups.json"))
}

pub fn read_sidebar_grouping() -> Result<SidebarGroupingState> {
    read_grouping_at(&state_path()?)
}

fn read_grouping_at(path: &Path) -> Result<SidebarGroupingState> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(err) => return Err(err).with_context(|| format!("failed to read {}", path.display())),
    };
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

/// Replace the grouping file atomically through a uniquely named temporary,
/// so concurrent writers can never interleave inside one temporary file.
fn write_grouping_at(path: &Path, state: &SidebarGroupingState) -> Result<()> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let mut encoded = serde_json::to_string_pretty(state)?;
    encoded.push('\n');
    let mut temporary = tempfile::Builder::new()
        .prefix(".sidebar-groups.")
        .suffix(".tmp")
        .tempfile_in(directory)
        .with_context(|| {
            format!(
                "failed to create a temporary file in {}",
                directory.display()
            )
        })?;
    temporary
        .write_all(encoded.as_bytes())
        .with_context(|| format!("failed to write {}", temporary.path().display()))?;
    temporary
        .persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

/// Hold an exclusive `flock` on the grouping file's lock for as long as the
/// returned file lives. Every sidebar and every `sidebar-action` subprocess
/// edits the grouping, so each edit is a locked read-modify-write.
fn lock_grouping(path: &Path) -> Result<fs::File> {
    use std::os::unix::io::AsRawFd;
    let lock_path = path.with_extension("lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open {}", lock_path.display()))?;
    // SAFETY: `file` owns a live descriptor for the whole lock lifetime;
    // closing it releases the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("failed to lock {}", lock_path.display()));
    }
    Ok(file)
}

/// Apply `change` to the persisted grouping as one locked read-modify-write,
/// writing only when it actually changed something. Returns the resulting
/// state (whether or not it was written) with `change`'s own result. An error
/// from `change` leaves the file untouched.
pub fn update_sidebar_grouping<T>(
    change: impl FnOnce(&mut SidebarGroupingState) -> Result<T>,
) -> Result<(SidebarGroupingState, T)> {
    update_grouping_at(&state_path()?, change)
}

fn update_grouping_at<T>(
    path: &Path,
    change: impl FnOnce(&mut SidebarGroupingState) -> Result<T>,
) -> Result<(SidebarGroupingState, T)> {
    let _lock = lock_grouping(path)?;
    let before = read_grouping_at(path)?;
    let mut state = before.clone();
    let value = change(&mut state)?;
    if state != before {
        write_grouping_at(path, &state)?;
    }
    Ok((state, value))
}

/// One edit the sidebar makes to the persisted grouping. Each is applied to
/// the latest state on disk rather than overwriting it with the sidebar's
/// in-memory copy, so edits made meanwhile by another sidebar or a
/// `sidebar-action` are kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupingChange {
    /// Fold (`collapsed`) or unfold the group with this row identity.
    Fold { identity: String, collapsed: bool },
    /// Fold or unfold the Resumable group.
    ResumableExpanded(bool),
    /// Unfold every listed group.
    Unfold(Vec<String>),
    /// Record the automatic group order the sidebar derived for `mode`, as
    /// long as the stored order is still the `previous` one it started from.
    AutomaticOrder {
        mode: String,
        previous: Option<Vec<String>>,
        order: Vec<String>,
    },
    /// Switch to the next automatic grouping mode.
    CycleAutomatic,
}

impl GroupingChange {
    pub fn apply(&self, state: &mut SidebarGroupingState) {
        match self {
            Self::Fold {
                identity,
                collapsed: true,
            } => {
                state.collapsed.insert(identity.clone());
            }
            Self::Fold {
                identity,
                collapsed: false,
            } => {
                state.collapsed.remove(identity);
            }
            Self::ResumableExpanded(expanded) => state.resumable_expanded = *expanded,
            Self::Unfold(identities) => {
                for identity in identities {
                    state.collapsed.remove(identity);
                }
            }
            Self::AutomaticOrder {
                mode,
                previous,
                order,
            } => {
                if state.automatic_order.get(mode) == previous.as_ref() {
                    state.automatic_order.insert(mode.clone(), order.clone());
                }
            }
            Self::CycleAutomatic => state.automatic = state.automatic.next(),
        }
    }
}

/// Persist one sidebar edit (see [`GroupingChange`]) and return the merged
/// state now on disk.
pub fn save_grouping_change(change: &GroupingChange) -> Result<SidebarGroupingState> {
    update_sidebar_grouping(|state| {
        change.apply(state);
        Ok(())
    })
    .map(|(state, ())| state)
}

pub fn cycle_automatic_grouping() -> Result<AutomaticGrouping> {
    save_grouping_change(&GroupingChange::CycleAutomatic).map(|state| state.automatic)
}

fn group_id(name: &str, existing: &[ManualGroup]) -> String {
    let base: String = name
        .chars()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let base = if base.is_empty() { "group" } else { &base };
    let mut candidate = base.to_string();
    let mut suffix = 2;
    while existing.iter().any(|group| group.id == candidate) {
        candidate = format!("{base}-{suffix}");
        suffix += 1;
    }
    candidate
}

pub fn create_manual_group(name: &str, process: Option<&str>) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("manual group name cannot be empty");
    }
    update_sidebar_grouping(|state| {
        let id = group_id(name, &state.manual);
        state.manual.push(ManualGroup {
            id: id.clone(),
            name: name.to_string(),
            processes: process.into_iter().map(str::to_string).collect(),
        });
        Ok(id)
    })
    .map(|(_, id)| id)
}

pub fn add_process_to_manual_group(group_id: &str, process: &str) -> Result<()> {
    update_sidebar_grouping(|state| {
        let group = state
            .manual
            .iter_mut()
            .find(|group| group.id == group_id)
            .with_context(|| format!("manual group '{group_id}' no longer exists"))?;
        if !group.processes.iter().any(|name| name == process) {
            group.processes.push(process.to_string());
        }
        Ok(())
    })
    .map(drop)
}

pub(crate) fn rename_process_in_manual_groups(old_name: &str, new_name: &str) -> Result<()> {
    update_sidebar_grouping(|state| {
        for group in &mut state.manual {
            for process in &mut group.processes {
                if process == old_name {
                    *process = new_name.to_string();
                }
            }
            let mut seen = HashSet::new();
            group
                .processes
                .retain(|process| seen.insert(process.clone()));
        }
        Ok(())
    })
    .map(drop)
}

pub fn dissolve_manual_group(group_id: &str) -> Result<()> {
    update_sidebar_grouping(|state| {
        let before = state.manual.len();
        state.manual.retain(|group| group.id != group_id);
        if state.manual.len() == before {
            bail!("manual group '{group_id}' no longer exists");
        }
        state.collapsed.remove(&format!("manual:{group_id}"));
        Ok(())
    })
    .map(drop)
}

pub fn reorder_manual_group(group_id: &str, delta: isize) -> Result<()> {
    update_sidebar_grouping(|state| {
        let index = state
            .manual
            .iter()
            .position(|group| group.id == group_id)
            .with_context(|| format!("manual group '{group_id}' no longer exists"))?;
        let target = (index as isize + delta)
            .clamp(0, state.manual.len().saturating_sub(1) as isize) as usize;
        state.manual.swap(index, target);
        Ok(())
    })
    .map(drop)
}

pub fn reorder_automatic_group(mode: AutomaticGrouping, key: &str, delta: isize) -> Result<()> {
    update_sidebar_grouping(|state| {
        let Some(order) = state.automatic_order.get_mut(mode.state_key()) else {
            return Ok(());
        };
        let Some(index) = order.iter().position(|candidate| candidate == key) else {
            return Ok(());
        };
        let target =
            (index as isize + delta).clamp(0, order.len().saturating_sub(1) as isize) as usize;
        order.swap(index, target);
        state
            .automatic_custom_order
            .insert(mode.state_key().to_string());
        Ok(())
    })
    .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_temp_home(test: impl FnOnce()) {
        let _guard = crate::ENV_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let original = std::env::var_os("HOME");
        let temporary = tempfile::tempdir().unwrap();
        // SAFETY: ENV_LOCK serializes the repository's HOME-mutating tests.
        unsafe { std::env::set_var("HOME", temporary.path()) };
        test();
        match original {
            // SAFETY: still protected by ENV_LOCK.
            Some(home) => unsafe { std::env::set_var("HOME", home) },
            // SAFETY: still protected by ENV_LOCK.
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn concurrent_edits_from_many_writers_are_all_kept() {
        with_temp_home(|| {
            let threads: Vec<_> = (0..8)
                .map(|index| {
                    std::thread::spawn(move || {
                        for round in 0..5 {
                            save_grouping_change(&GroupingChange::Fold {
                                identity: format!("auto:cwd:{index}-{round}"),
                                collapsed: true,
                            })
                            .unwrap();
                        }
                    })
                })
                .collect();
            for thread in threads {
                thread.join().unwrap();
            }
            let state = read_sidebar_grouping().unwrap();
            assert_eq!(state.collapsed.len(), 40, "no fold was lost");
            // No temporaries are left behind.
            let directory = state_path().unwrap();
            let leftovers: Vec<_> = fs::read_dir(directory.parent().unwrap())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
                .collect();
            assert!(leftovers.is_empty());
        });
    }

    #[test]
    fn unchanged_edits_do_not_rewrite_the_file() {
        with_temp_home(|| {
            let path = state_path().unwrap();
            save_grouping_change(&GroupingChange::ResumableExpanded(false)).unwrap();
            assert!(!path.exists(), "a no-op edit writes nothing");
            save_grouping_change(&GroupingChange::ResumableExpanded(true)).unwrap();
            let written = fs::metadata(&path).unwrap().modified().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(20));
            save_grouping_change(&GroupingChange::ResumableExpanded(true)).unwrap();
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
            // A failed edit leaves the file alone too.
            assert!(add_process_to_manual_group("missing", "x").is_err());
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), written);
        });
    }

    #[test]
    fn automatic_order_is_only_recorded_over_the_order_it_was_derived_from() {
        with_temp_home(|| {
            let change =
                |previous: Option<&[&str]>, order: &[&str]| GroupingChange::AutomaticOrder {
                    mode: "cwd".to_string(),
                    previous: previous.map(|keys| keys.iter().map(|key| key.to_string()).collect()),
                    order: order.iter().map(|key| key.to_string()).collect(),
                };
            save_grouping_change(&change(None, &["a", "b"])).unwrap();
            // Another sidebar recorded a different order meanwhile: a stale
            // derivation does not overwrite it.
            let state = save_grouping_change(&change(None, &["b", "a"])).unwrap();
            assert_eq!(state.automatic_order["cwd"], ["a", "b"]);
            let state = save_grouping_change(&change(Some(&["a", "b"]), &["b", "a"])).unwrap();
            assert_eq!(state.automatic_order["cwd"], ["b", "a"]);
        });
    }

    #[test]
    fn manual_membership_is_additive_and_persistent() {
        with_temp_home(|| {
            let id = create_manual_group("Focus", Some("alpha")).unwrap();
            add_process_to_manual_group(&id, "beta").unwrap();
            add_process_to_manual_group(&id, "alpha").unwrap();
            rename_process_in_manual_groups("beta", "renamed").unwrap();
            let state = read_sidebar_grouping().unwrap();
            assert_eq!(state.manual[0].name, "Focus");
            assert_eq!(state.manual[0].processes, ["alpha", "renamed"]);
        });
    }

    #[test]
    fn automatic_mode_and_both_order_spaces_persist_independently() {
        with_temp_home(|| {
            let first = create_manual_group("First", None).unwrap();
            let second = create_manual_group("Second", None).unwrap();
            reorder_manual_group(&second, -1).unwrap();
            save_grouping_change(&GroupingChange::AutomaticOrder {
                mode: "agent".to_string(),
                previous: None,
                order: vec!["claude".to_string(), "codex".to_string()],
            })
            .unwrap();
            reorder_automatic_group(AutomaticGrouping::Agent, "codex", -1).unwrap();
            assert_eq!(
                cycle_automatic_grouping().unwrap(),
                AutomaticGrouping::Window
            );

            let state = read_sidebar_grouping().unwrap();
            assert_eq!(state.manual[0].id, second);
            assert_eq!(state.manual[1].id, first);
            assert_eq!(state.automatic_order["agent"], ["codex", "claude"]);
            assert!(state.automatic_custom_order.contains("agent"));
            assert_eq!(state.automatic, AutomaticGrouping::Window);
        });
    }
}
