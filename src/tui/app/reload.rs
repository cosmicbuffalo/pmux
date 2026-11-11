//! Hot reload: noticing a replaced pmux binary and carrying sidebar state
//! across the re-exec.

use super::*;

/// Metadata which changes whenever an installed executable is replaced or
/// rewritten. On Unix, device/inode make an atomic `rename` detectable even
/// when the replacement happens to have the same size and timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ExecutableFingerprint {
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ExecutableFingerprint {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("read sidebar executable metadata for {}", path.display()))?;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        })
    }
}

/// Detect a completed replacement of the executable that is rendering this
/// sidebar. The watcher intentionally needs only metadata: the fresh binary is
/// selected again by `resolve_pmux_binary()` immediately before re-exec, which
/// also handles Linux's `/proc/self/exe ... (deleted)` spelling.
#[derive(Debug)]
pub(in crate::tui) struct SidebarBinaryWatcher {
    path: PathBuf,
    installed: ExecutableFingerprint,
    candidate: Option<(ExecutableFingerprint, Instant)>,
    last_checked: Instant,
}

impl SidebarBinaryWatcher {
    pub(in crate::tui) fn for_running_binary(now: Instant) -> Option<Self> {
        let path = crate::resolve_pmux_binary().ok()?;
        Self::new(path, now).ok()
    }

    pub(in crate::tui) fn new(path: PathBuf, now: Instant) -> Result<Self> {
        Ok(Self {
            installed: ExecutableFingerprint::read(&path)?,
            path,
            candidate: None,
            // Let the first check occur after a complete polling interval, not
            // immediately when the alternate screen is first painted.
            last_checked: now,
        })
    }

    /// Return true only when a replacement has stayed stable through the
    /// debounce window. A missing/unreadable file is treated as an in-progress
    /// install and retried later rather than taking down the live sidebar.
    pub(in crate::tui) fn reload_ready(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_checked) < SIDEBAR_BINARY_POLL {
            return false;
        }
        self.last_checked = now;

        let Ok(current) = ExecutableFingerprint::read(&self.path) else {
            self.candidate = None;
            return false;
        };
        if current == self.installed {
            self.candidate = None;
            return false;
        }

        match &self.candidate {
            Some((candidate, changed_at))
                if candidate == &current
                    && now.duration_since(*changed_at) >= SIDEBAR_BINARY_RELOAD_DEBOUNCE =>
            {
                true
            }
            Some((candidate, _)) if candidate == &current => false,
            _ => {
                self.candidate = Some((current, now));
                false
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum SidebarLoopExit {
    Reload(SidebarReloadState),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::tui) struct SidebarReloadState {
    selected_process: Option<String>,
    pub(in crate::tui) open_drawers: Vec<String>,
    open_all: bool,
    pub(in crate::tui) header_selected: bool,
}

impl SidebarReloadState {
    pub(in crate::tui) fn from_environment() -> Self {
        Self {
            selected_process: std::env::var(SIDEBAR_RELOAD_SELECTED_ENV)
                .ok()
                .filter(|name| !name.is_empty()),
            open_drawers: std::env::var(SIDEBAR_RELOAD_OPEN_DRAWERS_ENV)
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default(),
            // Compatibility with a sidebar binary that re-executes during an
            // upgrade from the former global drawer toggle.
            open_all: std::env::var(SIDEBAR_RELOAD_DRAWERS_ENV).as_deref() == Ok("1"),
            header_selected: std::env::var(SIDEBAR_RELOAD_HEADER_SELECTED_ENV).as_deref()
                == Ok("1"),
        }
    }

    pub(in crate::tui) fn capture(app: &SidebarApp) -> Self {
        let mut open_drawers: Vec<String> = app.open_drawers.iter().cloned().collect();
        open_drawers.sort();
        Self {
            selected_process: app.selected_process_name().map(str::to_string),
            open_drawers,
            open_all: false,
            header_selected: app.header_selected,
        }
    }

    pub(in crate::tui) fn restore(&self, app: &mut SidebarApp, list_height: u16) {
        if let Some(selected) = self.selected_process.as_deref()
            && let Some(process_index) = app
                .processes
                .iter()
                .position(|process| process.process.name == selected)
        {
            app.selected = if app.rows.is_empty() {
                process_index
            } else {
                app.rows
                    .iter()
                    .position(|row| {
                        matches!(row, SidebarListRow::Process { process_index: index, .. } if *index == process_index)
                    })
                    .unwrap_or(app.selected)
            };
        }
        app.open_drawers = self.open_drawers.iter().cloned().collect();
        if self.open_all {
            app.open_drawers.extend(
                app.processes
                    .iter()
                    .map(|process| process.process.name.clone()),
            );
        }
        app.header_selected = self.header_selected;
        app.ensure_drawer_fetch();
        if !app.header_selected {
            app.ensure_selected_item_visible(list_height);
        }
    }

    pub(in crate::tui) fn apply_to_command(&self, command: &mut Command) {
        command.env(
            SIDEBAR_RELOAD_SELECTED_ENV,
            self.selected_process.as_deref().unwrap_or_default(),
        );
        command.env(
            SIDEBAR_RELOAD_DRAWERS_ENV,
            if self.open_all { "1" } else { "0" },
        );
        command.env(
            SIDEBAR_RELOAD_OPEN_DRAWERS_ENV,
            serde_json::to_string(&self.open_drawers).unwrap_or_else(|_| "[]".to_string()),
        );
        command.env(
            SIDEBAR_RELOAD_HEADER_SELECTED_ENV,
            if self.header_selected { "1" } else { "0" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::test_support::*;

    #[test]
    fn sidebar_binary_watcher_reloads_only_after_a_stable_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("pmux");
        fs::write(&binary, b"old binary").unwrap();
        let started = Instant::now();
        let mut watcher = SidebarBinaryWatcher::new(binary.clone(), started).unwrap();

        let replacement = directory.path().join("pmux.new");
        // Keep the length identical: Unix's inode check is what proves this is
        // a replacement rather than just a size/mtime update.
        fs::write(&replacement, b"new binary").unwrap();
        fs::rename(replacement, &binary).unwrap();

        assert_ne!(
            watcher.installed,
            ExecutableFingerprint::read(&binary).unwrap()
        );
        assert!(!watcher.reload_ready(started + SIDEBAR_BINARY_POLL));
        assert!(
            watcher.reload_ready(started + SIDEBAR_BINARY_POLL + SIDEBAR_BINARY_RELOAD_DEBOUNCE)
        );
    }

    #[test]
    fn hot_reload_restores_selected_process_and_drawer_state_by_name() {
        let mut original = SidebarApp::new(None);
        let mut selected = test_process(Some("/selected"), None);
        selected.process.name = "selected".to_string();
        original.processes = vec![test_process(Some("/first"), None), selected];
        original.selected = 1;
        open_test_drawers(&mut original, &["selected"]);

        let state = SidebarReloadState::capture(&original);
        let mut restored = SidebarApp::new(None);
        let mut restored_selected = test_process(Some("/selected"), None);
        restored_selected.process.name = "selected".to_string();
        // Deliberately use a different order to prove the stable name, rather
        // than the old numeric index, controls restoration.
        restored.processes = vec![restored_selected, test_process(Some("/first"), None)];
        state.restore(&mut restored, 20);

        assert_eq!(restored.selected_name(), Some("selected"));
        assert!(restored.drawer_is_open(0));
        assert!(!restored.drawer_is_open(1));
    }
}
