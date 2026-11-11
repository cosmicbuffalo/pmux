//! Discovering where pmux processes are attached in the outer tmux session,
//! and the process lists the sidebar and picker display.

use super::pane::ensure_sidebar;
use super::*;

/// Returns the manually assigned session name, or `None` for pmux-generated
/// names. Presentation layers should use a neutral label for the latter so
/// implementation names never leak into the UI.
pub fn display_session_name(name: &str) -> Option<&str> {
    (!is_auto_generated_name(name)).then_some(name)
}

// The recorded host pane's location, taken from an already-fetched listing of
// this outer session's panes (see `outer_panes`). Unlike
// `location_for_session`, this spawns no tmux command, so the sidebar can
// resolve every detached process on each refresh at no per-process cost.
fn recorded_location(
    name: &str,
    context: &SidebarContext,
    panes_by_id: &HashMap<&str, &OuterPaneLocation>,
) -> Option<OuterPaneLocation> {
    // A host pane id is only meaningful on the outer server that wrote it.
    if read_session_host_socket(name).ok().flatten().as_deref() != Some(context.socket.as_str()) {
        return None;
    }
    let pane_id = read_session_host_pane(name).ok().flatten()?;
    // The listing covers only this outer session, which is also the filter
    // `location_for_session` applies.
    panes_by_id
        .get(pane_id.as_str())
        .map(|location| (*location).clone())
}

fn location_for_session(name: &str, context: &SidebarContext) -> Result<Option<OuterPaneLocation>> {
    // A host pane id is only meaningful on the outer server that wrote it.
    // Do not show an ambiguous legacy mapping in another server/session.
    if read_session_host_socket(name)?.as_deref() != Some(context.socket.as_str()) {
        return Ok(None);
    }
    let Some(pane_id) = read_session_host_pane(name)? else {
        return Ok(None);
    };
    let raw = match outer_tmux_output(
        &context.socket,
        &[
            "display-message",
            "-p",
            "-t",
            &pane_id,
            "#{session_id}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_index}",
        ],
        "query outer tmux process location",
    ) {
        Ok(raw) => raw,
        // The host pane may have been closed before its advisory mapping was
        // refreshed. Treat that as unattached rather than breaking the UI.
        Err(_) => return Ok(None),
    };
    let mut fields = raw.splitn(5, '\t');
    let session_id = fields.next().unwrap_or_default();
    if session_id != context.outer_session_id {
        return Ok(None);
    }
    let Some(window_id) = fields.next().filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let Some(window_index) = fields.next().filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let window_name = fields.next().unwrap_or_default();
    let pane_index = fields.next().unwrap_or_default();
    Ok(Some(OuterPaneLocation {
        pane_id,
        pane_index: pane_index.to_string(),
        window_id: window_id.to_string(),
        window_index: window_index.to_string(),
        window_name: window_name.to_string(),
    }))
}

/// Enrich a JSON state snapshot for display in the current outer tmux
/// session. Every pmux process remains represented, including non-agent
/// processes; outer location is optional metadata only.
pub fn sidebar_processes(
    snapshot: &StateSnapshot,
    context: &SidebarContext,
) -> Vec<SidebarProcess> {
    // One list-panes for the whole refresh, shared by attachment detection and
    // the recorded-host-pane fallback (previously one display-message per
    // detached process on every snapshot).
    let panes = outer_panes(context).unwrap_or_default();
    let panes_by_id: HashMap<&str, &OuterPaneLocation> = panes
        .iter()
        .map(|(_, location)| (location.pane_id.as_str(), location))
        .collect();
    let mut attachments = outer_attachments(&panes).unwrap_or_default();
    let mut processes = snapshot
        .sessions
        .iter()
        .map(|process| {
            let outer_locations = attachments.remove(&process.name).unwrap_or_default();
            if outer_locations.is_empty() {
                // No live client mapped to this session: fall back to the
                // recorded host pane, which also covers platforms or setups
                // where the client process tree cannot be inspected.
                return SidebarProcess {
                    process: process.clone(),
                    outer_location: recorded_location(&process.name, context, &panes_by_id),
                    outer_locations,
                };
            }
            let recorded_pane = read_session_host_pane(&process.name).ok().flatten();
            let outer_location = outer_locations
                .iter()
                .find(|location| location.window_id == context.host_window_id)
                .or_else(|| {
                    outer_locations
                        .iter()
                        .find(|location| Some(&location.pane_id) == recorded_pane.as_ref())
                })
                .or_else(|| outer_locations.first())
                .cloned();
            SidebarProcess {
                process: process.clone(),
                outer_location,
                outer_locations,
            }
        })
        .collect::<Vec<_>>();
    sort_sidebar_processes(&mut processes);
    processes
}

/// Every outer pane in this session that hosts a pmux client, keyed by the
/// pmux session that client is attached to.
///
/// A process can be attached from several outer panes at once (e.g. opened in
/// a second window). The recorded host pane only tracks the most recently
/// focused one, so instead each pmux client is traced up its process tree to
/// the outer pane it runs in. `None` when either server cannot be queried.
// Every pane in the current outer session with its process id and location.
fn outer_panes(context: &SidebarContext) -> Option<Vec<(u32, OuterPaneLocation)>> {
    let raw = outer_tmux_query(
        &context.socket,
        &[
            "list-panes",
            "-s",
            "-t",
            &context.outer_session_id,
            "-F",
            "#{pane_pid}\t#{pane_id}\t#{window_id}\t#{window_index}\t#{window_name}\t#{pane_index}",
        ],
        "list outer tmux panes for pmux attachments",
    )
    .ok()?;
    Some(
        raw.lines()
            .filter_map(|line| {
                let mut fields = line.splitn(6, '\t');
                let pid = fields.next()?.parse().ok()?;
                let pane_id = fields.next().filter(|value| !value.is_empty())?;
                let window_id = fields.next().filter(|value| !value.is_empty())?;
                let window_index = fields.next().filter(|value| !value.is_empty())?;
                let window_name = fields.next().unwrap_or_default();
                let pane_index = fields.next().unwrap_or_default();
                Some((
                    pid,
                    OuterPaneLocation {
                        pane_id: pane_id.to_string(),
                        pane_index: pane_index.to_string(),
                        window_id: window_id.to_string(),
                        window_index: window_index.to_string(),
                        window_name: window_name.to_string(),
                    },
                ))
            })
            .collect(),
    )
}

fn outer_attachments(
    outer: &[(u32, OuterPaneLocation)],
) -> Option<HashMap<String, Vec<OuterPaneLocation>>> {
    let output = crate::util::output_with_timeout(
        crate::tmux::tmux_cmd().args(["list-clients", "-F", "#{client_pid}\t#{session_name}"]),
        SIDEBAR_QUERY_TIMEOUT,
    )
    .ok()
    .filter(|output| output.status.success())?;
    let clients: Vec<(u32, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let (pid, session) = line.split_once('\t')?;
            Some((pid.trim().parse().ok()?, session.to_string()))
        })
        .filter(|(_, session)| !session.is_empty())
        .collect();
    if clients.is_empty() {
        return Some(HashMap::new());
    }
    let panes: HashMap<u32, &OuterPaneLocation> = outer
        .iter()
        .map(|(pid, location)| (*pid, location))
        .collect();
    let pane_pids: HashSet<u32> = panes.keys().copied().collect();
    let client_pids: Vec<u32> = clients.iter().map(|(pid, _)| *pid).collect();
    let hosts = crate::util::nearest_ancestors_in(&client_pids, &pane_pids);
    let mut attachments: HashMap<String, Vec<OuterPaneLocation>> = HashMap::new();
    for (client_pid, session) in clients {
        if let Some(location) = hosts.get(&client_pid).and_then(|pid| panes.get(pid)) {
            attachments
                .entry(session)
                .or_default()
                .push((*location).clone());
        }
    }
    let numeric = |value: &str| value.parse::<u64>().unwrap_or(u64::MAX);
    for locations in attachments.values_mut() {
        locations.sort_by(|left, right| {
            (numeric(&left.window_index), numeric(&left.pane_index))
                .cmp(&(numeric(&right.window_index), numeric(&right.pane_index)))
        });
        locations.dedup_by(|left, right| left.pane_id == right.pane_id);
    }
    Some(attachments)
}

/// Build the process list for the attach/switch picker. Identical ordering to
/// [`sidebar_processes`], but with no `outer_location` (the picker has no
/// outer-tmux context) so it never queries the outer tmux server.
pub fn picker_processes(snapshot: &StateSnapshot) -> Vec<SidebarProcess> {
    let mut processes = snapshot
        .sessions
        .iter()
        .map(|process| SidebarProcess {
            process: process.clone(),
            outer_location: None,
            outer_locations: Vec::new(),
        })
        .collect::<Vec<_>>();
    sort_sidebar_processes(&mut processes);
    processes
}

/// Sort processes for display: newest-started first (descending `created_at`),
/// oldest at the bottom, with the process name as a stable tiebreak. This is
/// deliberately independent of activity/attention signals, so the list does not
/// re-order itself while processes run — a stable order the user can rely on.
/// Shared by the sidebar and the picker so both list processes identically.
fn sort_sidebar_processes(processes: &mut [SidebarProcess]) {
    processes.sort_by(|left, right| {
        right
            .process
            .created_at
            .cmp(&left.process.created_at)
            .then_with(|| left.process.name.cmp(&right.process.name))
    });
}

/// Return the agent label shown at the start of a process's second sidebar row.
pub fn sidebar_agent_tag(process: &SidebarProcess) -> &str {
    process
        .process
        .agent
        .as_ref()
        .map(|agent| agent.kind.as_str())
        .unwrap_or("process")
}

/// Return the CWD displayed beside a process name, preferring the live CWD
/// and falling back to the recorded launch directory.
pub fn sidebar_process_cwd(process: &SidebarProcess) -> Option<&str> {
    process
        .process
        .current_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            process
                .process
                .launch_cwd
                .as_deref()
                .filter(|path| !path.trim().is_empty())
        })
}

/// Return the attachment text pinned beneath a process's first-row arrow.
#[cfg(test)]
pub fn sidebar_attachment_detail(
    process: &SidebarProcess,
    context: &SidebarContext,
) -> Option<String> {
    sidebar_attachment_detail_at(process, process.outer_location.as_ref(), context)
}

/// [`sidebar_attachment_detail`] for one specific attachment of a process, as
/// shown on each of its rows when it is attached in several windows.
pub fn sidebar_attachment_detail_at(
    process: &SidebarProcess,
    location: Option<&OuterPaneLocation>,
    context: &SidebarContext,
) -> Option<String> {
    match location {
        Some(location) if location.window_id == context.host_window_id => {
            Some(format!("pane {}", location.pane_index))
        }
        Some(location) => Some(format!("window {}", location.window_index)),
        None if process.process.attached_clients > 0 => {
            Some(format!("{} attached", process.process.attached_clients))
        }
        None => None,
    }
}

/// Focus the already-attached pane for a process in this outer tmux session.
pub fn jump_to_outer_pane(name: &str, context: &SidebarContext) -> Result<()> {
    let Some(location) = location_for_session(name, context)? else {
        bail!("process '{name}' is not attached in this outer tmux session");
    };
    if location.window_id != context.host_window_id {
        // A cross-window jump should retain the process dashboard in the
        // window the user lands in. It is opened detached so the target
        // process, rather than the new sidebar, receives focus below.
        ensure_sidebar(&location.pane_id, &context.socket)?;
    }
    outer_tmux_output(
        &context.socket,
        &["select-window", "-t", &location.window_id],
        "focus process window",
    )?;
    outer_tmux_output(
        &context.socket,
        &["select-pane", "-t", &location.pane_id],
        "focus process pane",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_are_never_used_as_sidebar_labels() {
        assert_eq!(display_session_name("1772262306-9rtuxxxx"), None);
        assert_eq!(
            display_session_name("manual-refactor"),
            Some("manual-refactor")
        );
    }

    // Regression: the sidebar resolved every detached process's recorded host
    // pane with its own `tmux display-message` on each snapshot. It now reads
    // them from the one pane listing taken per refresh.
    #[test]
    fn recorded_host_panes_resolve_from_the_shared_listing() {
        let mut env = crate::EnvGuard::new();
        let home = tempfile::tempdir().unwrap();
        env.set("HOME", home.path());
        let context = SidebarContext {
            socket: "/tmp/outer.sock".to_string(),
            host_pane: "%1".to_string(),
            outer_session_id: "$1".to_string(),
            host_window_id: "@1".to_string(),
            sidebar_on_right: false,
        };
        let listed = OuterPaneLocation {
            pane_id: "%7".to_string(),
            pane_index: "1".to_string(),
            window_id: "@2".to_string(),
            window_index: "2".to_string(),
            window_name: "work".to_string(),
        };
        let panes_by_id: HashMap<&str, &OuterPaneLocation> = HashMap::from([("%7", &listed)]);
        crate::state::write_session_host_pane("here", "%7", Some("/tmp/outer.sock")).unwrap();
        crate::state::write_session_host_pane("elsewhere", "%7", Some("/tmp/other.sock")).unwrap();
        crate::state::write_session_host_pane("closed", "%9", Some("/tmp/outer.sock")).unwrap();

        let resolve = |name| super::recorded_location(name, &context, &panes_by_id);
        assert_eq!(resolve("here"), Some(listed.clone()));
        // A pane id recorded on another outer server must not match this one.
        assert_eq!(resolve("elsewhere"), None);
        // A host pane that no longer exists in this session is unattached.
        assert_eq!(resolve("closed"), None);
        assert_eq!(resolve("never-attached"), None);
    }

    #[test]
    fn processes_sort_newest_started_first() {
        let process = |name: &str, created_at| SidebarProcess {
            process: ProcessInfo {
                name: name.to_string(),
                pane_id: "%1".to_string(),
                lifecycle: "running".to_string(),
                created_at,
                // Deliberately inverse to created_at: sorting must ignore
                // activity so the list stays stable while processes run.
                last_activity_at: 1_000_000 - created_at,
                attached_clients: 0,
                current_command: "zsh".to_string(),
                start_command: None,
                current_path: None,
                launch_command: None,
                launch_cwd: None,
                agent: None,
                startup: None,
                auto: None,
                spawned_by: None,
            },
            outer_location: None,
            outer_locations: Vec::new(),
        };

        let mut processes = vec![
            process("old", 10),
            process("newest", 30),
            process("mid", 20),
        ];
        sort_sidebar_processes(&mut processes);
        let order: Vec<&str> = processes
            .iter()
            .map(|entry| entry.process.name.as_str())
            .collect();
        assert_eq!(order, vec!["newest", "mid", "old"]);
    }
}
